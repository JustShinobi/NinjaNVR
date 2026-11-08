# Câmeras, Protocolos de Hardware & Sirene

O **NinjaNVR** integra nativamente fluxos RTSP/WebRTC (`go2rtc`), controle PTZ/ONVIF (`WS-UsernameToken PasswordDigest`), ajustes de hardware por protocolo (`camera_protocols.rs`) e **acionamento sonoro de alarme/sirene e áudio bidirecional (Push-to-Talk)** diretamente nos alto-falantes das câmeras suportadas.

## 1. Famílias de Protocolo Suportadas (`hardware.protocol`)

O registro declarativo (`CAMERA_PROTOCOL_REGISTRY`) detecta automaticamente ou permite configurar explicitamente a família da câmera em `cameras.<nome>.hardware.protocol`:

| Protocolo (`id`) | Fabricantes / Modelos | Porta ONVIF | Canal de Áudio / Sirene | Requisitos Extras |
| --- | --- | --- | --- | --- |
| `tapo` | TP-Link Tapo (`C200`, `C210`, `C220`, `C510W`, `C520WS`) | `:2020` (`/onvif/service`) | TCP `:8800` (`Streamd` `POST /stream` + MPEG-TS `audio/mp2t` G.711 A-law) | `hardware.cloud_password` (Senha da conta TP-Link ID) |
| `dahua_cgi` | Intelbras Mibo (`iME-360-C`, `iM3`, `iM4`, `iM5`), Dahua, Amcrest, Imou | `:80` (`/onvif/ptz_service`) | HTTP Digest `:80` (`POST /cgi-bin/audio.cgi?action=postAudio` G.711 A-law) + `BeepEnable` CGI | Usa a mesma conta/senha local do RTSP |
| `hikvision_isapi` | Hikvision, HiLook, Annke, Ezviz | `:80` | ONVIF / ISAPI | Conta local ONVIF |
| `reolink` | Reolink (`RLC-*`, `E1`) | `:8000` | ONVIF | Conta local ONVIF |
| `axis_vapix` | Axis Communications | `:80` | ONVIF / VAPIX | Conta local ONVIF |
| `onvif` | ONVIF Profile S / T genérico | `:80` | ONVIF | Conta local ONVIF |

---

## 2. Fluxo de Autenticação e Disparo de Sirene por Arquitetura

### 2.1 TP-Link Tapo (`tapo` — Porta `:2020` ONVIF + Porta `:8800` Streamd + Porta `:443` TPAP)

Nas câmeras **TP-Link Tapo** (ex.: `Tapo C200` HW 5.0 FW 1.5.7), os serviços são divididos entre duas credenciais distintas:

1. **Conta da Câmera (Local RTSP / ONVIF):**
   - Criada no app Tapo em *Configurações Avançadas ➔ Conta da Câmera*.
   - Autentica os streams **RTSP (`:554/stream1` e `:554/stream2`)** e o servidor **ONVIF (`:2020/onvif/service`)** via `WS-Security PasswordDigest`.
   - Expõe PTZ (`ContinuousMove`, `RelativeMove`, `AbsoluteMove`, `Stop`, `GetStatus`), Presets de hardware (`GetPresets`, `SetPreset`, `GotoPreset`, `RemovePreset`) e eventos `PullPoint`.
   - **Limitação do ONVIF Tapo:** O servidor ONVIF na porta `:2020` não expõe a sirene interna nem saída de áudio (`GetRelayOutputs` retorna `ter:ActionNotSupported` e `GetAudioOutputs` é vazio).

2. **Senha da Conta Cloud TP-Link ID (`hardware.cloud_password`):**
   - Necessária para autenticar na porta **`:8800` (`Streamd`)** e na porta **`:443` (`TPAP / SPAKE2+`)** com o usuário fixo `admin`.
   - **Como o NinjaNVR dispara a sirene na Tapo (`send_tapo_streamd_alaw_audio`):**
     1. Abre conexão TCP para `<ip>:8800` e envia `POST /stream HTTP/1.1` (`Content-Type: multipart/mixed; boundary=--client-stream-boundary--`) para obter o desafio `WWW-Authenticate: Digest realm="TP-Link IP-Camera", algorithm="MD5", encrypt_type="3", qop="auth", nonce="..."`.
     2. Calcula o hash da senha da conta TP-Link: quando `encrypt_type="3"`, usa `SHA256(cloud_password)` em hexadecimal maiúsculo; caso contrário, `MD5(cloud_password)` em maiúsculo.
     3. Abre uma nova conexão TCP autenticada com `Authorization: Digest username="admin", ...` (`HTTP/1.0 200 OK` + cabeçalho `Key-Exchange`).
     4. Solicita uma sessão de fala enviando o bloco multipart JSON:
        ```json
        {"type":"request","seq":3,"params":{"talk":{"mode":"aec"},"method":"get"}}
        ```
        e recebe `{"type":"response","seq":3,"params":{"error_code":0,"session_id":"..."}}`.
     5. Sintetiza a sirene tática de dois tons (`750 Hz – 1450 Hz` a `8000 Hz` mono em `G.711 A-law`), encapsula em pacotes **MPEG-TS de 188 bytes** (`PAT` no `PID 0x0000` ➔ `PMT` no `PID 0x1000` ➔ `PES` `stream_type 0x90` no `PID 0x0100`) e transmite em quadros `Content-Type: audio/mp2t` (`X-If-Encrypt: 0`, `X-Session-Id: <id>`) cadenciados a cada `20 ms`.

### 2.2 Intelbras / Dahua (`dahua_cgi` — Porta `:80` HTTP Digest CGI)

Nas câmeras **Intelbras Mibo / Dahua** (ex.: `iME-360-C`), toda a integração utiliza apenas a senha `admin` local (zero dependência de conta cloud):

- **Disparo imediato de sirene / talkback:** Envia o payload `G.711 A-law` (`8000 Hz`) via HTTP Digest `POST /cgi-bin/audio.cgi?action=postAudio&httptype=singlepart&channel=1` (`Content-Type: Audio/G.711A`).
- **Sirene nativa vinculada a eventos de hardware:** Configurável via `configManager.cgi?action=setConfig` nas tabelas `MotionDetect[0].EventHandler.BeepEnable=true` (`siren_on_motion`) e `AudioDetect[0].EventHandler.BeepEnable=true` (`siren_on_audio`).

---

## 3. Exemplo de Configuração (`config.yml`)

```yaml
cameras:
  escritorio:
    enabled: true
    onvif:
      host: 192.0.2.69
      port: 2020
      user: cam_operator
      password: "camera_local_password"
    hardware:
      protocol: tapo
      profile_token: profile_1
      video_source_token: raw_vs1
      cloud_password: "tp_link_cloud_password"
    lines:
      cantinho:
        points: [0.23, 0.20, 0.43, 0.21]
        direction: a_to_b
        objects: [person]
        siren: true
    zones:
      area_restrita:
        coordinates: "0.10,0.15,0.85,0.15,0.85,0.90,0.10,0.90"
        objects: [person]
        siren: true
```

---

## 4. Endpoints da API HTTP

- **`GET /api/{camera}/hardware`**
  Retorna o protocolo detectado, identidade do dispositivo, estado atual e matriz `capabilities` (incluindo `manual_siren`, `siren_on_event` e `push_to_talk`).
- **`POST /api/{camera}/hardware`**
  - Configurar a `cloud_password` em câmeras Tapo:
    ```json
    { "cloud_password": "sua_senha_tp_link" }
    ```
  - Disparar a sirene manualmente (teste ou acionamento imediato pelo operador):
    ```json
    { "trigger_siren": true, "siren_duration_sec": 3.5 }
    ```
- **`POST /api/{camera}/audio/talkback`**
  Recebe áudio PCM 16-bit LE, WAV ou G.711 A-law (binário ou Base64) e transmite em tempo real para o alto-falante da câmera (via `:8800` nas Tapo ou `/cgi-bin/audio.cgi` nas Intelbras/Dahua).
