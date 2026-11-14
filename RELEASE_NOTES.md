Reliability fixes from a full review of the code, and a peak meter that no longer misses overs.

- **Changed:** the level meter's peak reading now catches overs that fall between samples near the
  top of the audio band, where it could read well under the true level before.
- **Fixed:** a damaged settings file could wipe your saved presets. It is now set aside and kept
  instead, and settings are saved in a way an interrupted write can't damage.
- **Fixed:** exporting a correction, or running the self-test, while a band was soloed or isolated
  used the audition instead of your correction.
- **Fixed:** the instruments stayed black after a graphics driver reset until CAGEq was restarted.
- **Fixed:** with Equalizer APO, leaving an isolate audition briefly played a loud, filtered fade.
- Many smaller fixes, mostly to CAGEq's own engine, its setup and the safety mute.
- **Engine update:** several of the fixes are in CAGEq's own audio engine. The app flags the update
  after updating: Continue refreshes it, with a brief machine-wide audio interruption.
