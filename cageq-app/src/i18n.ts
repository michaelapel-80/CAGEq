import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./locales/en.json";
import de from "./locales/de.json";

/**
 * i18n setup. Language resolution: an explicit saved choice wins, otherwise the OS/browser
 * locale (any `de*` → German), otherwise English. Adding a region = drop a `xx.json` in
 * `locales/`, import it, and add it to `resources` + {@link LANGS}.
 *
 * Numbers/units (dB, Hz, Q) stay in engineering dot notation across every locale by design —
 * only words are translated (see filter.md; keeps ScrubNumber parsing locale-independent).
 */
export const LANGS = [
  { code: "en", label: "English" },
  { code: "de", label: "Deutsch" },
] as const;

export type LangCode = (typeof LANGS)[number]["code"];

const saved = localStorage.getItem("cageq-lang");
const detected = navigator.language?.toLowerCase().startsWith("de") ? "de" : "en";

void i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    de: { translation: de },
  },
  lng: (saved as LangCode) ?? detected,
  fallbackLng: "en",
  interpolation: { escapeValue: false }, // React already escapes output
  returnNull: false,
});

/** Switch language and persist the choice (mirrors the theme toggle). */
export function setLang(code: LangCode) {
  void i18n.changeLanguage(code);
  localStorage.setItem("cageq-lang", code);
}

export default i18n;
