# Detecção, Zonas, Linhas & Alarme Sonoro

O pipeline de visão computacional do **NinjaNVR** (`ninjanvr-vision`, `ninjanvr-infer`, `ninjanvr-track` e `ninjanvr-server::pipeline`) combina detecção de movimento SIMD em YUV420p, inferência de objetos (OpenVINO / ONNX), rastreamento multi-estágio (`ByteTracker`) e regras espaciais por **Zonas Poligonais (`zones`)** e **Linhas Virtuais / Tripwires (`lines`)**.

## 1. Zonas Poligonais (`cameras.<camera>.zones`)

Uma zona define um polígono normalizado (`0.0..1.0`) onde a presença ou permanência (`loitering_time`) de objetos rastreados gera alertas e pode acionar automaticamente a sirene da câmera:

| Campo | Tipo | Padrão | Descrição |
| --- | --- | --- | --- |
| `coordinates` | `string` | — | Coordenadas normalizadas `"x1,y1,x2,y2,x3,y3,..."` (mínimo 3 vértices). |
| `objects` | `string[]` | `[]` | Classes que ativam a zona (ex.: `["person", "car"]`). Vazio permite qualquer objeto rastreado. |
| `inertia` | `u32` | `3` | Quadros consecutivos necessários dentro da zona para confirmar a entrada. |
| `loitering_time` | `u32` | `0` | Tempo de permanência em segundos antes de gerar alerta (`0` = alerta imediato ao entrar). |
| `preset` | `string?` | `null` | Se definido, a zona só avalia objetos quando a PTZ estiver neste preset. |
| `siren` | `bool` | `false` | Quando `true`, dispara automaticamente a sirene sonora no alto-falante da câmera assim que um objeto permitido entrar na zona. |

## 2. Linhas de Travessia / Tripwires (`cameras.<camera>.lines`)

Uma linha virtual detecta o momento exato em que a base (pés/rodas) de um objeto rastreado cruza o segmento entre o ponto 1 e o ponto 2:

| Campo | Tipo | Padrão | Descrição |
| --- | --- | --- | --- |
| `points` | `[f32; 4]` | — | Coordenadas normalizadas `[x1, y1, x2, y2]` dos pontos 1 e 2 da linha. |
| `direction` | `string` | `"any"` | Sentido que gera o disparo: `"any"` (ambos), `"a_to_b"` (Lado A ➔ Lado B) ou `"b_to_a"` (Lado B ➔ Lado A). |
| `objects` | `string[]` | `[]` | Classes monitoradas na travessia (ex.: `["person"]`). |
| `siren` | `bool` | `false` | Quando `true`, dispara automaticamente a sirene sonora no alto-falante da câmera no instante em que um objeto permitido cruzar a linha no sentido configurado. |

---

## 3. Fluxo de Disparo Automático da Sirene (`siren: true`)

Quando `siren: true` está habilitado em uma **Zona** ou **Linha**:

1. **Avaliação no `ByteTracker` e `VisionPipeline`:**
   - Na entrada confirmada em uma zona (`entered_zones`) ou travessia de linha (`crossed_lines`), `process_lifecycle_events` verifica se:
     - O objeto não foi suprimido como falso positivo (`!suppressed_fp`);
     - A flag de alertas da câmera está ativa (`cam_rt_flags.alerts == true`);
     - O perfil operacional ativo (`armed`, `home`, `disarmed`) não suprime alertas para esta câmera (`!camera_suppressed_in_profile`);
     - A classe do objeto (`trk.label`) pertence à lista `objects` da zona/linha.
2. **Controle de Cooldown (Anti-Spam):**
   - `CameraDriverRuntime::try_claim_siren_trigger` aplica um cooldown de **15 segundos por câmera**, garantindo que múltiplos quadros ou objetos simultâneos não abram sessões concorrentes na porta `:8800` (Tapo `Streamd`) ou `:80` (Intelbras CGI).
3. **Telemetria e Eventos em Tempo Real:**
   - Registra um log estruturado em `ninjanvr.siren.<camera>`:
     `Triggered audible camera alarm on <camera> (trigger=line:<nome> | zone:<nome>, label=person)`
   - Publica um evento no WebSocket (`/ws`, tópico `"siren"`) com `{ camera, trigger, label, timestamp }`.
   - Dispara em background (`tokio::spawn`) a sirene sintetizada de `3.5s` (`750 Hz – 1450 Hz` G.711 A-law) no alto-falante da câmera.

---

## 4. Configuração pelo Console Web

1. **Habilitar a Sirene na Câmera:**
   - Em **Configurações ➔ Câmeras & Streams**, selecione a câmera:
     - **TP-Link Tapo (`tapo`):** Preencha a **Senha da Conta Cloud (`cloud_password`)** e clique em **Salvar**.
     - **Intelbras / Dahua (`dahua_cgi`):** Habilitada automaticamente com a senha ONVIF/RTSP.
   - Use o botão **Testar Sirene (3.5s)** no painel de Hardware (ou o botão **`ALM`** na gaveta **`HW`** do Live View) para validar o som no local.
2. **Vincular a uma Zona ou Linha:**
   - Em **Configurações ➔ Áreas, Linhas e Máscaras (`Zones Studio`)**, crie ou edite uma **Zona** ou **Linha**.
   - Marque a opção **"Disparar sirene sonora da câmera ao alertar"**.
   - Clique em **Revisar e Salvar** — a alteração entra em vigor imediatamente no loop de visão da câmera, sem necessidade de reiniciar o servidor.
