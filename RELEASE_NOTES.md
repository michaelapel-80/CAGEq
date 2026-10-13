Cleaner switching to Dry.

- **Fixed:** switching to or from Dry could leave a short audible disturbance on bass-heavy
  material, while an A/B switch between two slots was clean. CAGEq's own audio engine now
  handles Dry the same way as an A/B switch, fading over as long as the change needs instead
  of a fixed 10 ms.
- **Fixed:** after updating CAGEq's engine, the "engine needs updating" notice stayed up until
  dismissed or the app was restarted. It now disappears as soon as the update succeeds.
- The preamp readout is now shown on every chart view, not only Eq and Impulse.
- **Engine update:** the Dry fix is in CAGEq's own audio engine. The app flags the update after
  updating: Continue refreshes it, with a brief machine-wide audio interruption. The other
  changes are in the app itself. Equalizer APO users need nothing.
