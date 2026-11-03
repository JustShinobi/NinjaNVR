import React, { createContext, useContext, useState } from "react";
import { en, TranslationDict } from "./en";
import { pt } from "./pt";

export type SupportedLocale = "en" | "pt";
export { pt };

export const locales: Record<SupportedLocale, TranslationDict> = {
  en,
  pt,
};

export function resolveInitialLocale(): SupportedLocale {
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
