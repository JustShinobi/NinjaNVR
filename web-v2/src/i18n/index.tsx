import React, { createContext, useContext, useState } from "react";
import { en, TranslationDict } from "./en";

export type SupportedLocale = "en";

const locales: Record<SupportedLocale, TranslationDict> = {
  en,
};

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
  const [locale, setLocale] = useState<SupportedLocale>("en");
  return (
    <I18nContext.Provider value={{ locale, setLocale, t: locales[locale] }}>
      {children}
    </I18nContext.Provider>
  );
};

export const useI18n = () => useContext(I18nContext);
