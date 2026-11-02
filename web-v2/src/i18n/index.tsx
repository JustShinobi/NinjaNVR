import React, { createContext, useContext, useState } from "react";
import { en, TranslationDict } from "./en";

export type SupportedLocale = "en" | "pt";

const pt: TranslationDict = {
  brand: {
    name: "NINJANVR",
    release: "CONSOLE v2.0",
    engine: "RUST + OPENVINO",
    classicSwitch: "UI v1",
    classicSwitchHint: "Voltar para a interface clássica (v1)",
  },
  nav: {
    live: "Matriz Ao Vivo",
    sync: "Sync Playback",
    review: "Revisão & Triagem",
    explore: "Explorar Objetos",
    exports: "Exportações",
    zones: "Configurações",
    system: "Sistema & Logs",
  },
  auth: {
    title: "AUTENTICAÇÃO DO OPERADOR",
    subtitle: "Insira suas credenciais para acessar o Console NinjaNVR",
    username: "Usuário",
    password: "Senha",
    signIn: "Autenticar",
    signingIn: "Verificando...",
    signOut: "Sair",
    invalidCredentials: "Falha na autenticação. Verifique usuário e senha.",
  },
  live: {
    grid2x2: "Matriz 2x2",
    focusStage: "Foco + Timeline",
    singleFull: "Monitor Único",
    allCameras: "Todas as Câmeras",
    unreviewedAlerts: "Alertas Pendentes",
    hoverToPreview: "Passe o mouse para preview MP4 · Clique para abrir no Foco",
    markReviewed: "Marcar Visto",
    markAllReviewed: "Reconhecer Todos",
    noPendingAlerts: "Nenhum alerta pendente nas últimas 24 horas",
    liveBadge: "AO VIVO",
    timeShiftBadge: "TIME-SHIFT",
    returnToLive: "Voltar Ao Vivo",
    rewind10s: "-10s",
    rewind30s: "-30s",
    rewind2m: "-2m",
    export60s: "Clipe 60s",
    snapshot: "Foto",
    zonesOverlay: "Vetores",
    hdStream: "1080p Principal",
    subStream: "720p Sub",
    motionActive: "MOVIMENTO",
    idleState: "NOMINAL",
  },
  sync: {
    title: "MÁQUINA DO TEMPO MULTI-CÂMERA SINCRONIZADA",
    subtitle: "Reprodução simultânea de 4 canais travada em um relógio mestre compartilhado",
    masterClock: "Timestamp Mestre",
    jumpBack30: "-30s Todos",
    jumpBack10: "-10s Todos",
    jumpFwd10: "+10s Todos",
    jumpFwd30: "+30s Todos",
    recentIncidents: "Saltos Rápidos de Incidentes",
    syncLocked: "4 CANAIS SINCRONIZADOS",
  },
  review: {
    title: "BANCADA DE REVISÃO E TRIAGEM",
    alerts: "Alertas",
    detections: "Detecções",
    allSeverities: "Todas Severidades",
    unreviewedOnly: "Não Vistos",
    reviewedOnly: "Revisados",
    allStatuses: "Todos",
    markSelectedReviewed: "Marcar Visto",
    deleteSelected: "Excluir",
    exportClip: "Exportar MP4",
    keyboardHint: "[J/K] Nav · [R] Visto · [E] Clipe",
    emptyState: "Nenhum segmento de revisão corresponde aos filtros ativos.",
  },
  explore: {
    title: "EXPLORADOR DE OBJETOS RASTREADOS",
    searchPlaceholder: "Filtrar por ID, câmera, classe ou zona...",
    allLabels: "Todas Classes",
    favoritesOnly: "Apenas Retidos",
    topScore: "Confiança Máx",
    duration: "Duração",
    inProgress: "Ativo",
    retainToggle: "Reter",
    falsePositive: "Falso Positivo",
    viewSnapshot: "Snapshot",
    playClip: "Ver Clipe",
  },
  exports: {
    title: "ESTÚDIO E ARQUIVO DE EXPORTAÇÃO MP4",
    createTitle: "Gerar Novo Clipe MP4",
    camera: "Canal da Câmera",
    rangePreset: "Janela de Tempo",
    last1m: "Último 1 Minuto",
    last5m: "Últimos 5 Minutos",
    last15m: "Últimos 15 Minutos",
    customName: "Rótulo da Exportação",
    generateBtn: "Remuxar Clipe MP4",
    archiveTitle: "Arquivo de Clipes Exportados",
    downloadMp4: "Baixar MP4",
    renameBtn: "Renomear",
    deleteBtn: "Excluir",
    noExports: "Nenhum clipe MP4 exportado em disco.",
  },
  zones: {
    title: "ESTÚDIO TÁTICO DE CONFIGURAÇÕES E ZONAS",
    visualTab: "Inspetor de Zonas & Máscaras",
    yamlTab: "Estúdio config.yml Bruto",
    motionMasks: "Polígonos de Máscara de Movimento",
    detectionZones: "Zonas de Detecção",
    streamProbe: "Sonda de Stream RTSP",
    saveOnly: "Salvar Config",
    saveAndRestart: "Salvar & Reiniciar NVR",
    copyYaml: "Copiar YAML",
  },
  system: {
    title: "TELEMETRIA DO SISTEMA E LOGS EM TEMPO REAL",
    detectorLatency: "Latência de Inferência OpenVINO (ms)",
    cpuLoad: "Uso de CPU dos Processos (%)",
    memoryRss: "Consumo de Memória do Container (%)",
    storageVolumes: "Volumes de Armazenamento & Retenção",
    cameraHealth: "Telemetria de Pipeline por Câmera",
    liveLogs: "Stream de Logs de Serviço",
    pauseStream: "Pausar",
    resumeStream: "Retomar",
    copyLogs: "Copiar Buffer",
  },
};

const locales: Record<SupportedLocale, TranslationDict> = {
  en,
  pt,
};

function resolveInitialLocale(): SupportedLocale {
  if (typeof window === "undefined") return "en";
  const saved = window.localStorage?.getItem("ninjanvr_locale");
  if (saved === "pt" || saved === "en") return saved;
  return "en";
}

interface I18nContextValue {
  locale: SupportedLocale;
  setLocale: (loc: SupportedLocale) => void;
  t: TranslationDict;
}

const I18nContext = createContext<I18nContextValue>({
  locale: "en",
  setLocale: () => {},
  t: en,
});

export const I18nProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [locale, setLocaleState] = useState<SupportedLocale>(resolveInitialLocale);

  const setLocale = (loc: SupportedLocale) => {
    if (typeof window !== "undefined") {
      window.localStorage?.setItem("ninjanvr_locale", loc);
    }
    setLocaleState(loc);
  };

  return (
    <I18nContext.Provider value={{ locale, setLocale, t: locales[locale] }}>
      {children}
    </I18nContext.Provider>
  );
};

export const useI18n = () => useContext(I18nContext);
