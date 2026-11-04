voicetype - dictate into any window, on your own PC (nothing is sent to the cloud)

1. Run voicetype.exe. Windows may say "Windows protected your PC" because the
   app isn't signed: click "More info" -> "Run anyway".
2. Models: click Download next to "Base (English)" (148 MB). If you have an
   NVIDIA card, also download the GPU engine (670 MB) - it makes it instant.
3. Click into any text box and hold Ctrl + Alt + Space while you talk.
   Let go and the text is typed for you.

The pages on the left change how it works:
  Shortcut     pick new keys (click the box, press them), hold or press-to-start
  Hands-free   stop on silence, type each phrase as you pause, say "hey jarvis"
  Microphone   choose the mic and how loud counts as speech
  Output       paste or type keystrokes
  General      run at startup, tray, floating indicator

Closing the window keeps it running in the tray (bottom-right, by the clock);
right-click the tray icon -> Quit to exit. A floating pill at the bottom of the
screen shows when it's listening.

To uninstall: untick "Run voicetype at startup" (General), quit, delete the exe
and the %APPDATA%\voicetype folder (models and settings).
