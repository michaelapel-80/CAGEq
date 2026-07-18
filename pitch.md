# CAGEq – Caged Auto-Gain EQ
*(Arbeitstitel) · Kurzvorstellung für Mitwirkende und Tester*

> Diese Seite ist eine verkürzte, auf Überzeugung statt Vollständigkeit ausgelegte Fassung von [`filter.md`](filter.md), dem vollständigen Architektur-Dokument (ADD). Für Implementierungsdetails, offene Design-Fragen und die komplette Historie der Entscheidungen bitte dort nachlesen.

---

## Was ist CAGEq?

Eine moderne, eigenständige Windows-App zur Erstellung, Verwaltung und zum **lautheitsneutralen Vergleich** von parametrischen Equalizer-Filtern – auf Basis echter Kopfhörer-Messdaten. CAGEq übernimmt dabei die Steuerung von [Equalizer APO](https://sourceforge.net/projects/equalizerapo/), macht aber die eigentliche Filterberechnung, den A/B/Dry-Vergleich und die Sicherheitsmechanismen drumherum erst benutzbar.

Von der Idee her verwandt mit [AQUA](https://github.com/h39s/AQUA), aber eine unabhängige Neuentwicklung mit eigenem, bewusst anderem Tech-Stack – kein Fork, kein Codeübernahme.

## Das Problem

Wer seinen Kopfhörer per EQ korrigieren will, kennt das: Eine gute Korrekturkurve automatisch berechnen zu lassen ist inzwischen gelöst (danke, AutoEq) – aber sie *hörbar sinnvoll zu vergleichen* ist es nicht. Jede Kurve verändert auch die gefühlte Lautstärke, ein direkter A/B-Vergleich wird dadurch verfälscht ("klingt A nur lauter, oder wirklich besser?"). Und wer selbst Hand anlegt, riskiert ohne Schutzmechanismen schnell digitales Clipping oder unangenehme Pegelsprünge.

## Die Idee in Kürze

* **Lautheitsneutraler Vergleich (Auto-LUFS):** Jede Kurve bekommt automatisch einen Ausgleichs-Gain, berechnet nach menschlicher Gehörgewichtung (ITU-R BS.1770-4) – A/B/Dry unterscheiden sich dadurch nur noch in der Klangfarbe, nicht im Pegel.
* **Knackfreies Umschalten in Echtzeit** zwischen zwei eigenen Filter-Slots (A/B) und dem unbearbeiteten Original (Dry) – auch per Tastatur (A/S/D/W), damit man beim Hören nicht auf den Bildschirm schauen muss.
* **Clipping-Schutz, kaskadiert:** Mehrere unabhängige Sicherheitsstufen verhindern digitales Übersteuern – auch wenn mehrere Filter sich addieren und einzeln unauffällig aussehen.
* **Custom-Filter für alle, nicht nur Experten:** Wer will, setzt eigene parametrische Filterbänder von Hand (grafischer EQ, Chart-Drag oder präzise Zahleneingabe); wer nicht will, nutzt kuratierte oder selbst gespeicherte Presets per Klick.
* **Automatisch berechnete und eigene Filter bleiben getrennt**, werden aber zur Laufzeit sauber zusammengeführt – man verliert beim Nachjustieren nie die automatische Basiskorrektur.

## Sicherheit hat Vorrang

CAGEq folgt konsequent der Regel *"lieber kein Sound als falscher Sound"*: Bei jeder Unstimmigkeit schaltet das System sofort in einen definierten, stummen Sicherheitszustand – inklusive eines von der Haupt-Engine unabhängigen Watchdogs, der auch dann noch eingreifen kann, wenn die Berechnungs-Komponente selbst abgestürzt ist. Digitales Übersteuern (über 0 dBFS) ist durch mehrere unabhängige Prüfungen praktisch ausgeschlossen, nicht nur durch eine einzelne Rechnung.

## Wie es technisch funktioniert (für Mitentwickler)

Dreischichtige Desktop-Architektur:

```
Frontend (React/TypeScript) ──Tauri-Commands──▶ Rust-Core ──JSON-RPC über stdio──▶ Python-Sidecar
                                                     │                                    │
                                              Prozess-/Watchdog-                   AutoEq-Framework,
                                              Management                           NumPy/SciPy, DSP
                                                     └──────────── config.txt ───────────┘
                                                                      │
                                                        Equalizer APO (natives, klickfreies
                                                           10ms-Crossfading beim Reload)
```

* **Warum Python im Sidecar:** Das etablierte AutoEq-Framework (Fitting-Algorithmen + kuratierte Zielkurven-Datenbank) direkt nutzen, statt es in Rust/JS neu zu erfinden. Nicht latenzkritisch – die eigentliche Echtzeit-Überblendung übernimmt Equalizer APO selbst.
* **Warum Rust/Tauri:** Schlanke native WebView2-Shell statt gebündeltem Chromium (Electron), plus ein von Python unabhängiger Fail-Safe-Watchdog. Ehrlich gesagt: Keine Komponente hier braucht Rusts Rechenperformance wirklich – Rust ist hier auch bewusst als **Lernprojekt** gesetzt, im Gegensatz zum überwiegend AI-unterstützt erstellten Frontend soll der Rust-Core aktiv verstanden und mitgestaltet werden.
* **Ziellizenz:** GPL-3.0 (eigenständige Entscheidung, nicht durch Codeübernahme erzwungen).

## Aktueller Stand

* ✅ Vollständiges Architektur-Dokument (ADD, [`filter.md`](filter.md)) – Datenmodell, Algorithmen, IPC-Protokoll, Fail-Safe-Verhalten, UI/UX-Workflow, alles gegen Quellcode (AutoEq, Equalizer APO) verifiziert statt nur angenommen.
* ✅ Interaktives UI-Mockup ([`cage_dashboard_mockup.html`](cage_dashboard_mockup.html)) – kein Klickdummy, sondern klickbar mit echtem Verhalten: Diagramm-Drag, Custom-Filter anlegen/bearbeiten/löschen, A/B/Dry-Vergleich, Presets speichern/laden, alles inklusive Tastaturbedienung. Einfach im Browser öffnen und ausprobieren.
* ⏳ **Noch nicht gebaut:** der eigentliche Rust-Core, der Python-Sidecar und die echte Audio-Anbindung. Wir stehen am Ende der Design-Phase, am Anfang der Implementierung.

## Was noch offen ist – hier kommt ihr ins Spiel

1. **Ist der Standard-Lautstärke-Puffer (-9 dB) im Vergleichsmodus für echte Kopfhörer angenehm, oder zu leise/zu laut?** Reine Hörfrage, keine Quellcode-Frage – braucht echte Nutzertests mit unterschiedlichem Setup (v. a. Bluetooth-Kopfhörer ohne separaten Verstärker, die eigentliche Zielgruppe).
2. **Ist die geplante Rampen-Geschwindigkeit (6 dB/Sekunde) beim Umschalten in den finalen Lautstärke-Modus komfortabel genug?** Ebenfalls eine Hörfrage.
3. **Wie erkennt man zuverlässig, ob Equalizer APO für ein bestimmtes Wiedergabegerät überhaupt aktiviert ist?** Das steht nicht in der App-eigenen Konfiguration, sondern im Windows-Registry-/Installer-Zustand von Equalizer APO selbst – braucht gezielte Code-Recherche im EqAPO-Installer.

## Wie ihr mitmachen könnt

* **Mitentwickler:** Rust-Core (Sidecar-Lifecycle, Watchdog, IPC), Python/AutoEq-Integration, oder einfach kritischer Blick auf `filter.md` – jede Fehlannahme, die ihr vor der Implementierung findet, spart später Zeit. Frage 3 oben ist ein konkreter, in sich abgeschlossener Rechercheauftrag.
* **Tester:** Das Mockup öffnen, den vorgesehenen Workflow durchklicken (Messung importieren → Zielkurve wählen → A/B/Dry vergleichen → eigene Filter anpassen → Preset speichern) und sagen, wo es hakt oder unklar ist. Sobald echte Audio-Ausgabe läuft: Fragen 1 und 2 oben sind genau der Punkt, an dem eure Ohren mehr wert sind als jede weitere Spezifikation.

---

*Fragen, Feedback oder Lust auf einen Blick in den Code? Meldet euch – `filter.md` und das Mockup sind der aktuelle Stand, keine fertige Wahrheit.*
