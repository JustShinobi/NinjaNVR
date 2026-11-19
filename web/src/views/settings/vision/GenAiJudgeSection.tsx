import React, { useEffect, useRef, useState } from "react";
import { CheckCircle2, Sparkles, Trash2 } from "lucide-react";
import { CardCollapseToggle } from "../../../components/CardCollapseToggle";
import {
  FalsePositiveSignatureItem,
  GenAiConfig,
  NinjaConfigResponse,
} from "../../../types/nvr";
import {
  buildGenAiApiKeyPatch,
  buildGenAiProviderChangePatch,
  normalizeDiscoveredGenAiModels,
} from "../../../utils/configYaml";
import { formatDateTime } from "../../../utils/format";
import { ToggleSwitch } from "../ConsoleAndStreamsPanels";
import { useTwoStepConfirm } from "./useTwoStepConfirm";

export interface GenAiJudgeSectionProps {
  draftConfig: NinjaConfigResponse;
  updateDraft: (updater: (next: NinjaConfigResponse) => void) => void;
  scope: "global" | string;
  inspectorCam: string;
  isPt: boolean;
  notify: (msg: string) => void;
  initialDataCardsOpen: boolean;
  scopeDivider: (group: "global" | "camera") => React.ReactNode;
  isSafeGenAiCustomBaseUrl: (rawUrl: string) => boolean;
  filterFalsePositiveSignaturesByScope: (
    signatures: FalsePositiveSignatureItem[],
    scope: "global" | string,
    inspectorCam: string,
    filterMode: "scope" | "all"
  ) => FalsePositiveSignatureItem[];
  formatFalsePositiveReason: (
    reason?: string | null,
    dominantColor?: string | null,
    isPt?: boolean
  ) => { text: string; isTechnicalFallback: boolean };
  formatSignatureBbox: (normBbox?: [number, number, number, number] | null) => string;
  formatDhash64Hex: (dhash?: number | null) => string;
}

export const GenAiJudgeSection: React.FC<GenAiJudgeSectionProps> = ({
  draftConfig,
  updateDraft,
  scope,
  inspectorCam,
  isPt,
  notify,
  initialDataCardsOpen,
  scopeDivider,
  isSafeGenAiCustomBaseUrl,
  filterFalsePositiveSignaturesByScope,
  formatFalsePositiveReason,
  formatSignatureBbox,
  formatDhash64Hex,
}) => {
  const { isArmed, confirmOrArm } = useTwoStepConfirm(3000);
  const [fpSignatures, setFpSignatures] = useState<FalsePositiveSignatureItem[]>([]);
  const [fpLoading, setFpLoading] = useState(false);
  const [fpFilterMode, setFpFilterMode] = useState<"scope" | "all">("scope");
  const [showFpMemory, setShowFpMemory] = useState(initialDataCardsOpen);
  const [genaiModels, setGenaiModels] = useState<string[]>([]);
  const [genaiValidating, setGenaiValidating] = useState(false);
  const [genaiValidationState, setGenaiValidationState] = useState<{
    status: "idle" | "valid" | "invalid";
    message?: string;
  }>({ status: "idle" });
  const isMountedRef = useRef(true);
  const genaiValidateSeqRef = useRef(0);

  const genaiCfg: GenAiConfig = draftConfig.genai || {
    enabled: false,
    provider: "gemini",
    base_url: null,
    api_key: null,
    has_api_key: false,
    api_key_env: "GEMINI_API_KEY",
    model: "",
    verify_ambiguous_alerts: false,
    ambiguous_score_min: 0.45,
    ambiguous_score_max: 0.72,
    auto_suppress_false_positives: true,
    timeout_ms: 30000,
  };

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
      genaiValidateSeqRef.current += 1;
    };
  }, []);

  const loadFpSignatures = async () => {
    setFpLoading(true);
    try {
      const res = await fetch("/api/false_positives/signatures", {
        credentials: "include",
      });
      if (!isMountedRef.current) return;
      if (res.ok) {
        const rows = await res.json();
        if (isMountedRef.current && Array.isArray(rows)) {
          setFpSignatures(rows);
        }
      }
    } catch {
      // ignore
    } finally {
      if (isMountedRef.current) {
        setFpLoading(false);
      }
    }
  };

  useEffect(() => {
    void loadFpSignatures();
  }, []);

  const handleDeleteFpSignature = async (id: number) => {
    try {
      const res = await fetch(
        `/api/false_positives/signatures/${encodeURIComponent(String(id))}`,
        {
          method: "DELETE",
          credentials: "include",
        }
      );
      if (!isMountedRef.current) return;
      if (res.ok) {
        setFpSignatures((prev) => prev.filter((item) => item.id !== id));
        notify(`Deleted false-positive memory signature #${id}`);
      } else {
        notify(`Failed to delete false-positive signature #${id}`);
      }
    } catch {
      if (isMountedRef.current) {
        notify(`Network error while deleting false-positive signature #${id}`);
      }
    }
  };

  const updateGenAi = (patch: Partial<GenAiConfig>) => {
    updateDraft((next) => {
      next.genai = {
        ...genaiCfg,
        ...(next.genai || {}),
        ...patch,
      };
    });
  };

  const handleValidateGenAiApi = async () => {
    if (genaiValidating) return;
    const seq = ++genaiValidateSeqRef.current;
    setGenaiValidating(true);
    setGenaiValidationState({ status: "idle" });
    const prov = (genaiCfg.provider || "gemini").trim().toLowerCase();
    const directKey =
      genaiCfg.api_key && genaiCfg.api_key !== "***" && genaiCfg.api_key !== "*"
        ? genaiCfg.api_key.trim()
        : "";
    const customBase = (genaiCfg.base_url || "").trim().replace(/\/+$/, "");

    const applyValidModels = (models: string[], customMsg?: string) => {
      if (!isMountedRef.current || seq !== genaiValidateSeqRef.current) {
        return;
      }
      setGenaiModels(models);
      const msg =
        customMsg ||
        (isPt
          ? `API validada (${models.length} modelos disponíveis)`
          : `API validated (${models.length} models available)`);
      setGenaiValidationState({ status: "valid", message: msg });
      if (genaiCfg.model && !models.includes(genaiCfg.model)) {
        updateGenAi({ model: "" });
      }
      notify(msg);
    };

    try {
      const res = await fetch("/api/genai/models", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({
          provider: prov,
          api_key: directKey || undefined,
          api_key_env: genaiCfg.api_key_env || "GEMINI_API_KEY",
          base_url: customBase || null,
          clear_api_key: genaiCfg.clear_api_key || false,
        }),
      });
      const data = await res.json().catch(() => ({}));
      if (!isMountedRef.current || seq !== genaiValidateSeqRef.current) {
        return;
      }
      const loadedModels = normalizeDiscoveredGenAiModels(data, prov);
      if (res.ok && loadedModels.length > 0) {
        applyValidModels(loadedModels, data?.message);
        return;
      }

      const canUseDirectFallback =
        res.status !== 400 &&
        res.status !== 403 &&
        (!customBase || isSafeGenAiCustomBaseUrl(customBase));
      if (canUseDirectFallback && (directKey || prov === "ollama" || customBase)) {
        let directUrl = "";
        const headers: Record<string, string> = {};
        if (prov === "openai") {
          const base = customBase || "https://api.openai.com/v1";
          directUrl = base.endsWith("/models")
            ? base
            : base.endsWith("/v1")
            ? `${base}/models`
            : `${base}/v1/models`;
          if (directKey) headers.Authorization = `Bearer ${directKey}`;
        } else if (prov === "ollama") {
          const base = customBase || "http://127.0.0.1:11434";
          directUrl =
            base.endsWith("/api/tags") || base.endsWith("/models")
              ? base
              : `${base}/api/tags`;
          if (directKey) headers.Authorization = `Bearer ${directKey}`;
        } else {
          const base = customBase || "https://generativelanguage.googleapis.com";
          if (base.endsWith("/v1") || base.endsWith("/v1/models")) {
            directUrl = base.endsWith("/models") ? base : `${base}/models`;
            if (directKey) headers.Authorization = `Bearer ${directKey}`;
          } else {
            const endpoint = base.endsWith("/models")
              ? base
              : `${base}/v1beta/models`;
            directUrl = `${endpoint}?pageSize=100`;
            if (directKey) headers["x-goog-api-key"] = directKey;
          }
        }

        try {
          const directRes = await fetch(directUrl, { method: "GET", headers });
          const directJson = await directRes.json().catch(() => ({}));
          if (!isMountedRef.current || seq !== genaiValidateSeqRef.current) {
            return;
          }
          const directModels = normalizeDiscoveredGenAiModels(directJson, prov);
          if (directRes.ok && directModels.length > 0) {
            applyValidModels(directModels);
            return;
          }
          const apiErr =
            directJson?.error?.message ||
            data?.message ||
            (isPt
              ? `Falha ao validar API (HTTP ${directRes.status})`
              : `API validation failed (HTTP ${directRes.status})`);
          setGenaiModels([]);
          setGenaiValidationState({ status: "invalid", message: apiErr });
          notify(apiErr);
          return;
        } catch {
          // Cross-origin direct fallback blocked by CSP or network; preserve server error message below
          if (!isMountedRef.current || seq !== genaiValidateSeqRef.current) {
            return;
          }
        }
      }

      setGenaiModels([]);
      const errMsg =
        data?.message ||
        (isPt
          ? "Informe uma chave de API válida para carregar os modelos"
          : "Provide a valid API key to validate and load available models");
      setGenaiValidationState({ status: "invalid", message: errMsg });
      notify(errMsg);
    } catch {
      if (!isMountedRef.current || seq !== genaiValidateSeqRef.current) {
        return;
      }
      setGenaiModels([]);
      const errMsg = isPt
        ? "Erro de conexão ao validar API do provedor"
        : "Network error while validating provider API";
      setGenaiValidationState({ status: "invalid", message: errMsg });
      notify(errMsg);
    } finally {
      if (isMountedRef.current && seq === genaiValidateSeqRef.current) {
        setGenaiValidating(false);
      }
    }
  };

  const genaiModelOptions =
    genaiValidationState.status === "valid"
      ? genaiModels
      : genaiCfg.enabled && genaiCfg.model
      ? [genaiCfg.model]
      : [];

  const filteredFpSignatures = filterFalsePositiveSignaturesByScope(
    fpSignatures,
    scope,
    inspectorCam,
    fpFilterMode
  );

  return (
    <>
      {scopeDivider("global")}
      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "JUIZ DE IA VISUAL" : "VISION AI JUDGE"}</span>
            <span className="nvr-yaml-key-tag">genai</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span
              className={`nvr-badge ${
                genaiCfg.enabled && Boolean(genaiCfg.model)
                  ? "live"
                  : genaiCfg.enabled
                  ? "motion"
                  : ""
              }`}
            >
              <Sparkles size={11} />
              {(genaiCfg.provider || "gemini").toUpperCase()} ·{" "}
              {genaiCfg.model
                ? genaiCfg.model
                : isPt
                ? "MODELO NÃO SELECIONADO"
                : "NO MODEL SELECTED"}{" "}
              ·{" "}
              {genaiCfg.enabled
                ? genaiCfg.verify_ambiguous_alerts
                  ? "ACTIVE"
                  : isPt
                  ? "ATIVO (AUTO-VERIFICAÇÃO OFF)"
                  : "ACTIVE (AUTO-VERIFY OFF)"
                : isPt
                ? "EM ESPERA (OFF)"
                : "STANDBY (OFF)"}
            </span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div className="nvr-field-row" style={{ padding: "4px 0" }}>
            <div className="nvr-field-meta">
              <div className="nvr-field-label">
                {isPt
                  ? "Ativar Juiz de IA Visual (Gemini / OpenAI / Ollama Local)"
                  : "Enable Vision AI Judge (Gemini / OpenAI / Local Ollama)"}
              </div>
              <div className="nvr-field-desc">
                {isPt
                  ? "Fluxo guiado: 1. Provedor → 2. Credencial API (Chave direta ou Variável Env) → 3. Validar API → 4. Modelo Visual."
                  : "Guided flow: 1. Provider → 2. API Credential (Direct Key or Env Var fallback) → 3. Validate API → 4. Vision Model."}
              </div>
            </div>
            <ToggleSwitch
              checked={genaiCfg.enabled}
              label="Enable Vision AI Judge"
              onChange={(v) => updateGenAi({ enabled: v })}
            />
          </div>

          <div
            className={!genaiCfg.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{ display: "flex", flexDirection: "column", gap: 8 }}
          >
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
                gap: 8,
                alignItems: "end",
              }}
            >
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "1. Provedor LLM" : "1. Provider"}
                </div>
                <select
                  className="nvr-select mono"
                  aria-label="Vision AI Judge Provider"
                  style={{ width: "100%", marginTop: 2 }}
                  value={genaiCfg.provider || "gemini"}
                  onChange={(e) => {
                    genaiValidateSeqRef.current += 1;
                    setGenaiValidating(false);
                    updateGenAi(
                      buildGenAiProviderChangePatch(e.target.value, genaiCfg.base_url)
                    );
                    setGenaiModels([]);
                    setGenaiValidationState({ status: "idle" });
                  }}
                >
                  <option value="gemini">Google Gemini (gemini)</option>
                  <option value="openai">OpenAI / Compatible (openai)</option>
                  <option value="ollama">Local Ollama (ollama)</option>
                </select>
              </div>

              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "2A. Chave da API Direta" : "2A. Direct API Key"}
                  {genaiCfg.has_api_key ? (isPt ? " (Salva)" : " (Saved)") : ""}
                </div>
                <div style={{ display: "flex", gap: 4, marginTop: 2 }}>
                  <input
                    type="password"
                    className="nvr-input mono"
                    aria-label="Vision AI Judge API Key"
                    style={{ flex: 1, minWidth: 0, width: "100%" }}
                    placeholder={
                      genaiCfg.has_api_key
                        ? isPt
                          ? "•••••••• (chave salva)"
                          : "•••••••• (saved key)"
                        : genaiCfg.provider === "ollama"
                        ? isPt
                          ? "Opcional (Ollama local)"
                          : "Optional (Local Ollama)"
                        : isPt
                        ? `Cole a chave ou use ${genaiCfg.api_key_env || "GEMINI_API_KEY"}`
                        : `Paste key or use ${genaiCfg.api_key_env || "GEMINI_API_KEY"}`
                    }
                    value={
                      genaiCfg.api_key === "***" || genaiCfg.api_key === "*"
                        ? ""
                        : genaiCfg.api_key || ""
                    }
                    onChange={(e) => {
                      const val = e.target.value;
                      genaiValidateSeqRef.current += 1;
                      setGenaiValidating(false);
                      updateGenAi({
                        ...buildGenAiApiKeyPatch(
                          val,
                          genaiCfg.has_api_key,
                          false
                        ),
                        model: "",
                      });
                      setGenaiModels([]);
                      setGenaiValidationState({ status: "idle" });
                    }}
                  />
                  {(genaiCfg.has_api_key ||
                    (genaiCfg.api_key && genaiCfg.api_key !== "")) && (
                    <button
                      type="button"
                      className="nvr-btn danger mono"
                      style={{ padding: "0 7px", fontSize: "var(--text-xs)" }}
                      title={
                        isPt
                          ? "Limpar chave da API salva"
                          : "Clear saved Vision AI Judge API key"
                      }
                      aria-label="Clear Vision AI Judge API Key"
                      onClick={() => {
                        genaiValidateSeqRef.current += 1;
                        setGenaiValidating(false);
                        updateGenAi({
                          ...buildGenAiApiKeyPatch("", false, true),
                          model: "",
                        });
                        setGenaiModels([]);
                        setGenaiValidationState({ status: "idle" });
                      }}
                    >
                      ×
                    </button>
                  )}
                </div>
              </div>

              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "2B. Variável Env (Fallback)" : "2B. Fallback Env Var"}
                </div>
                <input
                  className="nvr-input mono"
                  aria-label="Vision AI Judge API Key Env"
                  style={{ width: "100%", marginTop: 2 }}
                  placeholder={
                    genaiCfg.provider === "openai"
                      ? "OPENAI_API_KEY"
                      : genaiCfg.provider === "ollama"
                      ? "OLLAMA_API_KEY"
                      : "GEMINI_API_KEY"
                  }
                  value={genaiCfg.api_key_env}
                  onChange={(e) => {
                    genaiValidateSeqRef.current += 1;
                    setGenaiValidating(false);
                    updateGenAi({ api_key_env: e.target.value });
                    setGenaiValidationState({ status: "idle" });
                  }}
                />
              </div>

              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt
                    ? "2C. Endpoint Base URL (Opcional)"
                    : "2C. Endpoint Base URL (Optional)"}
                </div>
                <input
                  className="nvr-input mono"
                  aria-label="Vision AI Judge Base URL"
                  style={{ width: "100%", marginTop: 2 }}
                  placeholder={
                    genaiCfg.provider === "ollama"
                      ? "http://127.0.0.1:11434"
                      : isPt
                      ? "Opcional (Proxy LLM / OpenRouter)..."
                      : "Optional endpoint override (LLM proxy)..."
                  }
                  value={genaiCfg.base_url || ""}
                  onChange={(e) => {
                    genaiValidateSeqRef.current += 1;
                    setGenaiValidating(false);
                    updateGenAi({
                      base_url: e.target.value.trim() ? e.target.value.trim() : null,
                    });
                    setGenaiValidationState({ status: "idle" });
                  }}
                />
              </div>
            </div>

            <div
              className="nvr-subcard"
              style={{
                padding: "8px 10px",
                display: "grid",
                gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))",
                gap: 10,
                alignItems: "end",
              }}
            >
              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "3. Validar API & Buscar Modelos" : "3. Validate API & Fetch Models"}
                </div>
                <button
                  type="button"
                  className={`nvr-btn mono ${
                    genaiValidationState.status === "valid" || Boolean(genaiCfg.model)
                      ? "success"
                      : "primary"
                  }`}
                  aria-label="Validate Vision AI Judge API"
                  style={{ marginTop: 2, whiteSpace: "nowrap" }}
                  disabled={genaiValidating}
                  onClick={() => void handleValidateGenAiApi()}
                >
                  <CheckCircle2 size={12} />
                  {genaiValidating
                    ? isPt
                      ? "Validando API..."
                      : "Validating API..."
                    : genaiValidationState.status === "valid"
                    ? isPt
                      ? `API Validada (${genaiModels.length} modelos)`
                      : `API Validated (${genaiModels.length} models)`
                    : genaiCfg.model
                    ? isPt
                      ? "Revalidar API & Atualizar Lista"
                      : "Re-Validate API & Refresh List"
                    : isPt
                    ? "Validar API & Carregar Modelos"
                    : "Validate API & Load Models"}
                </button>
              </div>

              <div>
                <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-secondary)" }}>
                  {isPt ? "4. Modelo Visual Ativo" : "4. Active Vision Model"}
                </div>
                <select
                  className="nvr-select mono"
                  aria-label="Vision AI Judge Model"
                  style={{ width: "100%", marginTop: 2 }}
                  disabled={genaiModelOptions.length === 0}
                  value={
                    genaiModelOptions.includes(genaiCfg.model)
                      ? genaiCfg.model
                      : ""
                  }
                  onChange={(e) => updateGenAi({ model: e.target.value })}
                >
                  <option value="">
                    {genaiModelOptions.length === 0
                      ? isPt
                        ? "— Valide a API no Passo 3 para carregar os modelos —"
                        : "— Validate API in Step 3 to load available models —"
                      : isPt
                      ? `— Escolha o Vision Model na lista (${genaiModelOptions.length} disponíveis) —`
                      : `— Select Vision Model from list (${genaiModelOptions.length} available) —`}
                  </option>
                  {genaiModelOptions.map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
                </select>
              </div>
            </div>

            {genaiValidationState.status !== "idle" &&
              genaiValidationState.message && (
                <div
                  className="mono"
                  style={{
                    padding: "5px 8px",
                    borderRadius: 3,
                    fontSize: "var(--text-xs)",
                    background:
                      genaiValidationState.status === "valid"
                        ? "rgba(16, 185, 129, 0.12)"
                        : "rgba(239, 68, 68, 0.12)",
                    border:
                      genaiValidationState.status === "valid"
                        ? "1px solid rgba(16, 185, 129, 0.4)"
                        : "1px solid rgba(239, 68, 68, 0.4)",
                    color:
                      genaiValidationState.status === "valid"
                        ? "var(--signal-live-text)"
                        : "var(--signal-alert-text)",
                  }}
                >
                  {genaiValidationState.message}
                </div>
              )}
          </div>
        </div>
      </div>

      <div className="nvr-cfg-card">
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <span>{isPt ? "AUTO-VERIFICAÇÃO & SUPRESSÃO" : "AUTO-VERIFY & SUPPRESSION"}</span>
            <span className="nvr-yaml-key-tag">genai.verify_ambiguous_alerts</span>
          </div>
        </div>
        <div className="nvr-cfg-card-body" style={{ padding: 10, gap: 8 }}>
          <div
            className={!genaiCfg.enabled ? "nvr-cfg-disabled-zone" : ""}
            style={{ display: "flex", flexDirection: "column", gap: 8 }}
          >
            <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label">
                    {isPt
                      ? "Auditar Alertas Ambíguos Automaticamente"
                      : "Auto-Verify Ambiguous Alerts"}
                  </div>
                  <div className="nvr-field-desc">
                    {isPt
                      ? `Aciona o Juiz de IA quando a pontuação cair na faixa [${Math.round(
                          (genaiCfg.ambiguous_score_min ?? 0.45) * 100
                        )}%, ${Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}%].`
                      : `Triggers Vision AI Judge when score falls in [${Math.round(
                          (genaiCfg.ambiguous_score_min ?? 0.45) * 100
                        )}%, ${Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}%].`}
                  </div>
                </div>
                <ToggleSwitch
                  checked={genaiCfg.verify_ambiguous_alerts}
                  label="Verify Ambiguous Alerts"
                  onChange={(v) => updateGenAi({ verify_ambiguous_alerts: v })}
                />
              </div>

              <div
                className={`nvr-field-row ${
                  !genaiCfg.verify_ambiguous_alerts ? "nvr-cfg-disabled-zone" : ""
                }`}
                style={{ borderBottom: "none", padding: "2px 0" }}
              >
                <div className="nvr-field-meta">
                  <div className="nvr-field-label">
                    {isPt
                      ? "Auto-Suprimir Falsos Positivos Rejeitados"
                      : "Auto-Suppress Rejected False Positives"}
                  </div>
                  <div className="nvr-field-desc">
                    {isPt
                      ? "Grava assinatura espacial + dhash64 no SQLite quando a IA rejeita o alerta."
                      : "Persists spatial IoU + dhash64 signature in SQLite when AI rejects a detection."}
                  </div>
                </div>
                <ToggleSwitch
                  checked={genaiCfg.auto_suppress_false_positives}
                  label="Auto-Suppress False Positives"
                  onChange={(v) => updateGenAi({ auto_suppress_false_positives: v })}
                />
              </div>
            </div>

            <div
              className={!genaiCfg.verify_ambiguous_alerts ? "nvr-cfg-disabled-zone" : ""}
              style={{ display: "flex", flexDirection: "column", gap: 4 }}
            >
              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label mono">
                    {isPt ? "Pontuação Ambígua Mín" : "Ambiguous Score Min"} (
                    {Math.round((genaiCfg.ambiguous_score_min ?? 0.45) * 100)}%)
                    {!genaiCfg.verify_ambiguous_alerts && (
                      <span className="nvr-yaml-key-tag">
                        {isPt ? "Auto-Verify desligado" : "Auto-Verify OFF"}
                      </span>
                    )}
                  </div>
                </div>
                <input
                  type="range"
                  min={25}
                  max={80}
                  disabled={!genaiCfg.verify_ambiguous_alerts}
                  aria-label="Ambiguous Score Minimum"
                  value={Math.round((genaiCfg.ambiguous_score_min ?? 0.45) * 100)}
                  onChange={(e) =>
                    updateGenAi({
                      ambiguous_score_min: Number(
                        (Number(e.target.value) / 100).toFixed(2)
                      ),
                    })
                  }
                />
              </div>

              <div className="nvr-field-row" style={{ borderBottom: "none", padding: "2px 0" }}>
                <div className="nvr-field-meta">
                  <div className="nvr-field-label mono">
                    {isPt ? "Pontuação Ambígua Máx" : "Ambiguous Score Max"} (
                    {Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}%)
                    {!genaiCfg.verify_ambiguous_alerts && (
                      <span className="nvr-yaml-key-tag">
                        {isPt ? "Auto-Verify desligado" : "Auto-Verify OFF"}
                      </span>
                    )}
                  </div>
                </div>
                <input
                  type="range"
                  min={40}
                  max={95}
                  disabled={!genaiCfg.verify_ambiguous_alerts}
                  aria-label="Ambiguous Score Maximum"
                  value={Math.round((genaiCfg.ambiguous_score_max ?? 0.72) * 100)}
                  onChange={(e) =>
                    updateGenAi({
                      ambiguous_score_max: Number(
                        (Number(e.target.value) / 100).toFixed(2)
                      ),
                    })
                  }
                />
              </div>
            </div>
          </div>
        </div>
      </div>

      <div
        className={`nvr-cfg-card ${showFpMemory ? "" : "is-collapsed"}`}
        style={{ gridColumn: "1 / -1" }}
      >
        <div className="nvr-cfg-card-header">
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <CardCollapseToggle
              open={showFpMemory}
              onToggle={() => setShowFpMemory((v) => !v)}
              title={isPt ? "MEMÓRIA DE FALSOS POSITIVOS" : "FALSE-POSITIVE MEMORY"}
              count={filteredFpSignatures.length}
              controlsId="nvr-fp-memory-body"
            />
            <span className="nvr-yaml-key-tag">IoU &gt;= 0.72 · dhash64 &lt;= 12</span>
            <span className="nvr-yaml-key-tag">SQLite Live DB</span>
          </div>
          <div style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            {scope !== "global" && (
              <div style={{ display: "flex", alignItems: "center", gap: 4 }}>
                <button
                  type="button"
                  className={`nvr-btn mono ${fpFilterMode === "scope" ? "active" : ""} sm`}
                  onClick={() => setFpFilterMode("scope")}
                >
                  {inspectorCam.toUpperCase()} (
                  {
                    filterFalsePositiveSignaturesByScope(
                      fpSignatures,
                      scope,
                      inspectorCam,
                      "scope"
                    ).length
                  }
                  )
                </button>
                <button
                  type="button"
                  className={`nvr-btn mono ${fpFilterMode === "all" ? "active" : ""} sm`}
                  onClick={() => setFpFilterMode("all")}
                >
                  {isPt ? "TODAS" : "ALL CAMERAS"} ({fpSignatures.length})
                </button>
              </div>
            )}
            <button
              type="button"
              className="nvr-btn mono sm"
              onClick={() => void loadFpSignatures()}
            >
              {fpLoading
                ? isPt
                  ? "Atualizando..."
                  : "Refreshing..."
                : isPt
                ? "Atualizar"
                : "Refresh"}
            </button>
          </div>
        </div>
        {showFpMemory && (
          <div id="nvr-fp-memory-body" className="nvr-cfg-card-body" style={{ padding: 10, gap: 6 }}>
            {filteredFpSignatures.length === 0 ? (
              <div className="mono" style={{ fontSize: "var(--text-xs)", color: "var(--text-muted)" }}>
                {fpSignatures.length > 0 && scope !== "global" && fpFilterMode === "scope"
                  ? isPt
                    ? `Nenhuma assinatura de falso positivo em ${inspectorCam.toUpperCase()}. Clique em "TODAS (${fpSignatures.length})" acima para ver assinaturas de outras câmeras.`
                    : `No false-positive signatures recorded on ${inspectorCam.toUpperCase()}. Click "ALL CAMERAS (${fpSignatures.length})" above to view signatures across other cameras.`
                  : isPt
                  ? "Nenhuma assinatura de falso positivo aprendida ainda. Marque um evento como Falso Positivo na aba Explorar ou Revisão para suprimir recorrências instantaneamente."
                  : "No learned false-positive signatures yet. Mark an event as False Positive in Review/Explore or via the Vision AI Judge to suppress recurring static triggers at 0ms."}
              </div>
            ) : (
              <div style={{ overflowX: "auto" }}>
                <table
                  className="mono"
                  style={{
                    width: "100%",
                    borderCollapse: "collapse",
                    fontSize: "var(--text-xs)",
                    fontVariantNumeric: "tabular-nums",
                  }}
                >
                  <thead>
                    <tr
                      style={{
                        textAlign: "left",
                        color: "var(--text-secondary)",
                        borderBottom: "1px solid var(--border-hairline)",
                      }}
                    >
                      <th style={{ padding: "4px 6px" }}>{isPt ? "CÂMERA" : "CAMERA"}</th>
                      <th style={{ padding: "4px 6px" }}>{isPt ? "CLASSE" : "LABEL"}</th>
                      <th style={{ padding: "4px 6px" }}>
                        {isPt ? "REGIÃO (NORM BBOX)" : "SPATIAL BBOX"}
                      </th>
                      <th style={{ padding: "4px 6px" }}>DHASH64</th>
                      <th style={{ padding: "4px 6px" }}>HITS</th>
                      <th style={{ padding: "4px 6px" }}>
                        {isPt ? "ÚLTIMA VEZ" : "LAST SEEN"}
                      </th>
                      <th style={{ padding: "4px 6px" }}>
                        {isPt ? "MOTIVO / AUDITORIA" : "REASON"}
                      </th>
                      <th style={{ padding: "4px 6px", textAlign: "right" }}>
                        {isPt ? "AÇÃO" : "ACTION"}
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {filteredFpSignatures.map((sig) => {
                      const bx = Number.isFinite(sig.norm_bbox?.[0])
                        ? Math.max(0, Math.min(0.95, sig.norm_bbox[0]))
                        : 0;
                      const by = Number.isFinite(sig.norm_bbox?.[1])
                        ? Math.max(0, Math.min(0.95, sig.norm_bbox[1]))
                        : 0;
                      const bw = Number.isFinite(sig.norm_bbox?.[2])
                        ? Math.max(0.05, Math.min(1 - bx, sig.norm_bbox[2]))
                        : 0.2;
                      const bh = Number.isFinite(sig.norm_bbox?.[3])
                        ? Math.max(0.05, Math.min(1 - by, sig.norm_bbox[3]))
                        : 0.2;
                      const formattedReason = formatFalsePositiveReason(
                        sig.reason,
                        sig.dominant_color,
                        isPt
                      );
                      const delKey = `delete-fp:${sig.id}`;
                      const delArmed = isArmed(delKey);
                      return (
                        <tr
                          key={sig.id}
                          style={{ borderBottom: "1px solid var(--border-hairline)" }}
                        >
                          <td style={{ padding: "5px 6px", fontWeight: 700, whiteSpace: "nowrap" }}>
                            {sig.camera.toUpperCase()}
                          </td>
                          <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                            {sig.label.toUpperCase()}
                          </td>
                          <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                            <div style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                              <svg
                                width="32"
                                height="18"
                                viewBox="0 0 32 18"
                                aria-hidden="true"
                                style={{
                                  background: "var(--bg-input)",
                                  border: "1px solid var(--border-strong)",
                                  borderRadius: 2,
                                  flexShrink: 0,
                                }}
                              >
                                <rect
                                  x={bx * 32}
                                  y={by * 18}
                                  width={Math.max(2, bw * 32)}
                                  height={Math.max(2, bh * 18)}
                                  fill="rgba(245, 158, 11, 0.35)"
                                  stroke="var(--signal-motion)"
                                  strokeWidth="1"
                                />
                              </svg>
                              <span>{formatSignatureBbox(sig.norm_bbox)}</span>
                            </div>
                          </td>
                          <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                            {formatDhash64Hex(sig.dhash64)}
                          </td>
                          <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                            {sig.hit_count}
                          </td>
                          <td style={{ padding: "5px 6px", whiteSpace: "nowrap" }}>
                            {formatDateTime(sig.last_seen_at)}
                          </td>
                          <td
                            title={formattedReason.text}
                            style={{
                              padding: "5px 6px",
                              color: formattedReason.isTechnicalFallback
                                ? "var(--text-muted)"
                                : "var(--text-secondary)",
                              maxWidth: 440,
                              overflow: "hidden",
                              textOverflow: "ellipsis",
                              whiteSpace: "nowrap",
                            }}
                          >
                            {formattedReason.text}
                          </td>
                          <td style={{ padding: "5px 6px", textAlign: "right" }}>
                            <button
                              type="button"
                              className="nvr-btn danger mono sm"
                              aria-label={`Delete false-positive signature ${sig.id}`}
                              onClick={() => {
                                confirmOrArm(delKey, () => {
                                  void handleDeleteFpSignature(sig.id);
                                });
                              }}
                            >
                              <Trash2 size={11} />
                              {delArmed ? (isPt ? "Confirmar?" : "Confirm?") : null}
                            </button>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </div>
        )}
      </div>
    </>
  );
};
