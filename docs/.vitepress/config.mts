import { defineConfig } from "vitepress";

export default defineConfig({
  title: "NinjaNVR",
  description: "Documentação oficial do NinjaNVR — AI Network Video Recorder & Console",
  lang: "pt-BR",
  base: "/NinjaNVR/",
  cleanUrls: true,
  appearance: "force-dark",

  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/NinjaNVR/logo.svg" }],
  ],

  themeConfig: {
    logo: "/logo.svg",
    siteTitle: "NinjaNVR Docs",

    nav: [
      { text: "Início", link: "/" },
      { text: "Primeiros Passos", link: "/getting-started/overview" },
      { text: "Configuração", link: "/configuration/cameras" },
      { text: "Console Web", link: "/console/live-and-sync" },
      { text: "Operação", link: "/operations/mqtt-home-assistant" },
    ],

    sidebar: [
      {
        text: "Primeiros Passos",
        items: [
          { text: "Visão Geral", link: "/getting-started/overview" },
          { text: "Instalação", link: "/getting-started/installation" },
          { text: "Guia Rápido", link: "/getting-started/quickstart" },
        ],
      },
      {
        text: "Configuração",
        items: [
          { text: "Câmeras & Streams", link: "/configuration/cameras" },
          { text: "Detecção & IA", link: "/configuration/detection" },
          { text: "Gravação & Retenção", link: "/configuration/storage" },
        ],
      },
      {
        text: "Console Web",
        items: [
          { text: "Matriz Ao Vivo & Sync", link: "/console/live-and-sync" },
          { text: "Timeline & Revisão", link: "/console/timeline-and-review" },
          { text: "Zonas & Máscaras", link: "/console/zones-and-masks" },
        ],
      },
      {
        text: "Operação & Integrações",
        items: [
          { text: "MQTT & Home Assistant", link: "/operations/mqtt-home-assistant" },
          { text: "Segurança & Autenticação", link: "/operations/security-and-auth" },
          { text: "Solução de Problemas", link: "/operations/troubleshooting" },
        ],
      },
    ],

    socialLinks: [
      { icon: "github", link: "https://github.com/JustShinobi/NinjaNVR" },
    ],

    search: {
      provider: "local",
    },

    outline: {
      label: "Nesta página",
    },

    docFooter: {
      prev: "Página anterior",
      next: "Próxima página",
    },

    footer: {
      message: "Distribuído sob a licença MIT.",
      copyright: "Copyright © NinjaNVR",
    },
  },
});
