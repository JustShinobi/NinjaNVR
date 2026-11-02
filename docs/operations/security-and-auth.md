# Segurança, Autenticação & SSO (OIDC)

O **NinjaNVR** oferece uma arquitetura de segurança em múltiplas camadas projetada tanto para redes locais isoladas quanto para exposições autenticadas via Internet, suportando login local com **PBKDF2-HMAC-SHA256**, **OpenID Connect (OIDC) SSO** nativo (**Zitadel**, **Authelia**, **Authentik**, **Keycloak**), **ForwardAuth de Proxy Confiável**, **Controle de Acesso Baseado em Função (RBAC)** por câmera e **Tokens de API Scoped (`nnvr_*`)**.

---

## 1. Portas de Rede e Modelo de Sessão

| Porta | Protocolo | Comportamento Padrão |
| :--- | :--- | :--- |
| **`:5000`** | HTTP | Porta interna de baixa latência (usada por serviços no mesmo pod/container ou rede interna confiável). |
| **`:8971`** | HTTPS / TLS | Porta externa autenticada (exige cookie JWT válido, sessão OIDC, cabeçalho ForwardAuth de proxy confiável ou `Bearer nnvr_*`). |

::: tip Certificado TLS Auto-Gerado Compatível com Apple (iOS/macOS)
Se nenhum certificado TLS customizado for montado, o NinjaNVR gera automaticamente um certificado ECDSA P-256 em memória/disco com `Subject Alternative Names (SAN)` (`DNS:localhost`, `IP:127.0.0.1`), `ExtendedKeyUsage::ServerAuth` e validade $\le 398$ dias.
:::

---

## 2. Single Sign-On (SSO) Nativo via OpenID Connect (OIDC)

O NinjaNVR possui cliente **OpenID Connect (OIDC)** nativo integrado no servidor Rust (`ninjanvr-server`) utilizando o fluxo **Authorization Code + PKCE (`S256`)**, sem depender de containers extras de `oauth2-proxy`.

### Fluxo de Autenticação OIDC
1. O operador acessa o console (ou clica em **Sign in with Zitadel / SSO** em `/login`).
2. Quando `auth.oidc.auto_redirect: true` está ativo, qualquer acesso não autenticado redireciona automaticamente para `/api/auth/oidc/login`.
3. O NinjaNVR consulta `/.well-known/openid-configuration` do provedor, gera um par criptográfico **PKCE (`S256`)** + `state` aleatório, grava um cookie `HttpOnly` temporário (`ninjanvr_oidc_pkce`) e redireciona o navegador para o endpoint de autorização do IdP.
4. Após o login no provedor, o callback `/api/auth/oidc/callback` valida o `state` e troca o `code` pelo `id_token` / `access_token` usando o `code_verifier` PKCE.
5. As *claims* do `id_token` e do endpoint `userinfo` (incluindo grupos OIDC e roles de projeto do Zitadel em `urn:zitadel:iam:org:project:roles`) são avaliadas para atribuir automaticamente o perfil RBAC (`admin`, `operator` ou `viewer`) e emitir o cookie JWT de sessão.

::: info Botão de Logout e Auto-Redirect
Quando você clica no botão **`Logout`** no canto superior direito do console, o NinjaNVR encerra a sessão JWT e exibe a tela de login sem disparar o `auto_redirect` imediatamente, permitindo trocar de conta ou entrar com credencial local de emergência.
:::

---

### Passo a Passo: Configurando com o Zitadel

#### Passo 1 — Criar a Aplicação OIDC no Zitadel
1. No console do **Zitadel**, abra seu Projeto (ex.: `Homelab`) e clique em **New Application**.
2. Defina o nome como **`NinjaNVR`** e selecione o tipo **User Agent (SPA)** ou **Web**.
3. Selecione o método de autenticação **PKCE** (`None (PKCE)` — não requer segredo estático — ou `POST` caso deseje usar `Client Secret`).
4. Em **Redirect URIs**, cadastre a URL pública do seu NinjaNVR seguida de `/api/auth/oidc/callback`:
   ```text
   https://nvr.example.com/api/auth/oidc/callback
   ```
   *(Marque também `Development Mode` se quiser permitir redirecionamentos adicionais em ambiente de laboratório).*
5. Após criar a aplicação, abra a aba **Token Settings** do aplicativo no Zitadel e marque:
   * ✅ **User roles inside ID Token** (`idTokenRoleAssertion`)
   * ✅ **User Info inside ID Token** (`idTokenUserinfoAssertion`)
6. Copie o **`Client ID`** gerado pelo Zitadel.

#### Passo 2 — Criar as Roles de Acesso no Projeto do Zitadel (Opcional)
Para que o Zitadel controle quem é `admin`, `operator` ou `viewer` dentro do NinjaNVR:
1. No seu Projeto do Zitadel, vá em **Roles** e adicione as chaves:
   * `admin` (`NinjaNVR Admin`)
   * `operator` (`NinjaNVR Operator`)
   * `viewer` (`NinjaNVR Viewer`)
2. Em **Authorizations (User Grants)**, atribua a role `admin` ao seu usuário.

#### Passo 3 — Ativar pelo Console do NinjaNVR (ou `config.yml`)
Você pode configurar visualmente sem editar arquivos manualmente:
1. No NinjaNVR Console, abra a aba **Settings (6)** $\rightarrow$ seção **MQTT, Auth & Users**.
2. No card **OPENID CONNECT (OIDC) SSO & TRUSTED-PROXY FORWARDAUTH**:
   * Ative o switch **`OIDC SSO`**;
   * Ative o switch **`Auto-Redirect`** se desejar pular a tela de login local e entrar direto via Zitadel;
   * **Provider Name:** `Zitadel`
   * **OIDC Issuer URL:** `https://auth.example.com` *(URL raiz da sua instância Zitadel, sem barra final)*
   * **OIDC Client ID:** cole o `Client ID` copiado no Passo 1;
   * **Default Role:** `viewer` (ou `admin` em ambientes pessoais onde todos os usuários autorizados no Zitadel são administradores).
3. Clique em **Save Config**.

#### Equivalente no `config.yml`:
```yaml
auth:
  enabled: true
  local_login_enabled: true
  oidc:
    enabled: true
    provider_name: "Zitadel"
    issuer_url: "https://auth.example.com"
    client_id: "123456789012345678"
    client_secret_env: "NINJANVR_OIDC_CLIENT_SECRET"
    scopes:
      - openid
      - profile
      - email
    auto_redirect: true
    username_claim: "preferred_username"
    groups_claim: "groups"
    default_role: "viewer"
    role_mapping:
      admin:
        - "admin"
        - "admins"
        - "ninjanvr-admin"
      operator:
        - "operator"
        - "operators"
        - "ninjanvr-operator"
      viewer:
        - "viewer"
        - "users"
        - "ninjanvr-viewer"
```

::: warning Segredo de Cliente OIDC (`NINJANVR_OIDC_CLIENT_SECRET`)
Se o seu provedor OIDC exigir `client_secret` (ex.: Authelia ou Zitadel em modo `client_secret_post`), **nunca** coloque o segredo no `config.yml`. Defina-o como variável de ambiente `NINJANVR_OIDC_CLIENT_SECRET` no container/Kubernetes Secret. Para aplicações Zitadel em modo **PKCE (`None`)**, nenhuma variável de segredo é necessária.
:::

---

## 3. ForwardAuth de Proxy Reverso Confiável (Traefik + Authelia / Tinyauth)

Se você protege seus serviços na borda com um middleware ForwardAuth no Traefik, Nginx ou Caddy:

```yaml
auth:
  enabled: true
  proxy:
    enabled: true
    trusted_proxies:
      - "127.0.0.1/32"
      - "10.42.0.0/16"
    user_header: "Remote-User"
    groups_header: "Remote-Groups"
    logout_url: "https://auth.example.com/logout"
```

* **Proteção Anti-Spoofing:** Os cabeçalhos `Remote-User` e `Remote-Groups` só são aceitos quando o IP TCP de origem da conexão pertence a um dos blocos CIDR listados em `trusted_proxies`. Requisições diretas de IPs fora dessa lista têm os cabeçalhos ignorados.

---

## 4. Controle de Acesso Baseado em Função (RBAC) e Escopo por Câmera

Cada operador (local ou via OIDC/Proxy) opera sob um dos 3 níveis de permissão:

| Role | Permissões no Console e API |
| :--- | :--- |
| **`admin`** | Acesso total: visualização, triagem, exportação, edição do `config.yml`, gerenciamento de usuários/tokens e proxy web de câmeras. |
| **`operator`** | Pode visualizar câmeras autorizadas, marcar alertas como revisados, adicionar notas forenses, alternar perfis (`ARMED`/`HOME`) e exportar clipes MP4. Bloqueado para alterações de sistema/configuração. |
| **`viewer`** | Acesso estritamente somente-leitura (`GET`/`HEAD`). A aba **Settings** fica oculta e mutações são bloqueadas pelo middleware HTTP. |

### Restrição de Câmeras por Usuário (`Allowed Cameras`)
Em **Settings $\rightarrow$ MQTT, Auth & Users $\rightarrow$ Operator Accounts**, você pode definir a coluna **`Allowed Cameras`** de qualquer operador ou visualizador como:
* `*` (todas as câmeras), ou
* Uma lista separada por vírgulas (ex.: `portao,garagem`). O backend filtra automaticamente `/api/config`, `/api/stats`, `/api/review`, `/api/events` e streams Live/VOD para que o usuário veja apenas as câmeras permitidas.

---

## 5. Tokens de API Scoped (`nnvr_*`) e Links Compartilháveis Assinados

### Tokens de API (`Bearer nnvr_*`)
Para integrações externas (Prometheus/VictoriaMetrics, scripts de automação, clientes MCP como OpenCode/Claude):
1. Vá em **Settings $\rightarrow$ MQTT, Auth & Users $\rightarrow$ Scoped API Bearer Tokens**.
2. Escolha o escopo:
   * `read_only`: permite apenas `GET /api/*` e consultas MCP de leitura;
   * `metrics`: restrito exclusivamente a `GET /api/stats` e `GET /api/metrics`;
   * `full`: permissão administrativa completa.
3. O token em texto puro (`nnvr_...`) é exibido **uma única vez** no momento da criação; o banco SQLite armazena apenas o hash **SHA-256** e o prefixo de identificação.

### Links Compartilháveis com Expiração (`/share/:token`)
Na aba **Review** ou **Exports**, clique em **`Share`** (`1h`, `24h` ou `7d`) para gerar uma URL auto-contida assinada via **HMAC-SHA256**. Quem receber o link consegue assistir exclusivamente àquele clipe em um player dedicado sem precisar de conta no NVR, até o instante exato de expiração codificado na assinatura.
