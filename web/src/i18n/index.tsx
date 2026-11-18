import React, { createContext, useContext, useEffect, useState } from "react";
import { en, TranslationDict } from "./en";
import { pt } from "./pt";

export type SupportedLocale = "en" | "pt";
export { pt };

export const locales: Record<SupportedLocale, TranslationDict> = {
  en,
  pt,
};

export function resolveHtmlLang(locale: SupportedLocale): "pt-BR" | "en" {
  return locale === "pt" ? "pt-BR" : "en";
}

export function applyLocaleToDocument(locale: SupportedLocale): "pt-BR" | "en" {
  const htmlLang = resolveHtmlLang(locale);
  if (typeof document !== "undefined" && document.documentElement) {
    document.documentElement.lang = htmlLang;
    if (typeof document.documentElement.setAttribute === "function") {
      document.documentElement.setAttribute("lang", htmlLang);
    }
  }
  return htmlLang;
}

export function resolveInitialLocale(): SupportedLocale {
  if (typeof window === "undefined") return "en";
  try {
    const saved = window.localStorage?.getItem("ninjanvr_locale");
    if (saved === "pt" || saved === "en") return saved;
  } catch {
    // ignore storage access errors
  }
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
  const [locale, setLocaleState] = useState<SupportedLocale>(() => {
    const initial = resolveInitialLocale();
    applyLocaleToDocument(initial);
    return initial;
  });

  useEffect(() => {
    applyLocaleToDocument(locale);
  }, [locale]);

  const setLocale = (loc: SupportedLocale) => {
    try {
      if (typeof window !== "undefined") {
        window.localStorage?.setItem("ninjanvr_locale", loc);
      }
    } catch {
      // ignore storage access errors
    }
    applyLocaleToDocument(loc);
    setLocaleState(loc);
  };

  return (
    <I18nContext.Provider value={{ locale, setLocale, t: locales[locale] }}>
      {children}
    </I18nContext.Provider>
  );
};

export const useI18n = () => useContext(I18nContext);
