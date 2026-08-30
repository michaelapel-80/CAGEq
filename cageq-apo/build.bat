@echo off
rem ---------------------------------------------------------------------------
rem Build CAGEqApo.dll (x64): Rust static library + the C++ COM shim.
rem
rem Building is harmless anywhere — it neither registers the COM server nor touches
rem any audio endpoint. Only scripts\register.ps1 does that, and only on the VM.
rem
rem Needs: VS2022 (C++ toolset) + Windows SDK, and cargo on PATH or at %USERPROFILE%.
rem Does NOT need the WDK: baseaudioprocessingobject.h / AudioBaseProcessingObjectV140.lib
rem ship in the plain SDK under um/ (this surprised the earlier spike, which assumed a
rem driver kit was required and gave up on the base class because of it).
rem ---------------------------------------------------------------------------
setlocal
cd /d "%~dp0"

set CARGO=cargo
where /q cargo || set CARGO=%USERPROFILE%\.cargo\bin\cargo.exe

rem 1) The Rust core -> ..\target\release\cageq_apo.lib
rem
rem +crt-static: link the CRT statically, so the finished DLL imports only OS libraries and
rem has no VC++ redistributable dependency. This matters more than usual here — the DLL is
rem loaded into audiodg.exe (LocalService, session 0), and a missing VCRUNTIME140.dll there
rem is a silent load failure that looks exactly like "our APO is broken". `cargo rustc`
rem rather than RUSTFLAGS so the flag applies to this crate alone.
"%CARGO%" rustc --release -p cageq-apo -- -C target-feature=+crt-static
if errorlevel 1 ( echo [cageq-apo] cargo build failed & exit /b 1 )

rem 2) The C++ shim, linked against it.
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" >nul
if errorlevel 1 ( echo [cageq-apo] vcvars64 failed & exit /b 1 )

if not exist build mkdir build

rem /GR- : the APO build runs without RTTI, matching EqualizerAPO and the base class.
rem /EHsc: new(std::nothrow) is used throughout, but the SDK headers still want EH on.
rem /MT  : REQUIRED, not stylistic — must match the Rust half's CRT choice (+crt-static
rem        above). Mixing static and dynamic CRT in one image is a documented way to get
rem        duplicate-symbol noise and two independent CRT states in the same process.
rem
rem /NODEFAULTLIB:atls.lib — audiomediatypecrt.lib embeds /DEFAULTLIB:atls.lib, but ATL is
rem        a separate VS Installer component many machines (including this one) don't have.
rem        We only use that lib's plain format helpers, which reference no ATL symbols, so
rem        dropping it links clean. If a future change *does* pull an ATL symbol in, this
rem        surfaces immediately as an unresolved external at link time — not at runtime
rem        inside audiodg — at which point install "C++ ATL for latest v143 build tools".
rem
rem The trailing libs are: the APO base class, the audio engine, then exactly what
rem `rustc --print native-static-libs` reports for the Rust staticlib. Keep that list in
rem sync if the Rust side grows dependencies — a missing one shows up as an unresolved
rem symbol at link time, not at runtime.
cl /nologo /LD /MT /EHsc /GR- /W4 /O2 /std:c++17 /DUNICODE /D_UNICODE ^
   /Fo:build\ ^
   shim\cageq_apo.cpp ^
   /Fe:build\CAGEqApo.dll ^
   /link /DEF:shim\cageq_apo.def /NODEFAULTLIB:atls.lib ^
   ..\target\release\cageq_apo.lib ^
   AudioBaseProcessingObjectV140.lib audioeng.lib audiomediatypecrt.lib ^
   ole32.lib oleaut32.lib advapi32.lib user32.lib ^
   kernel32.lib ntdll.lib userenv.lib ws2_32.lib dbghelp.lib
if errorlevel 1 ( echo [cageq-apo] link failed & exit /b 1 )

echo.
echo [cageq-apo] built build\CAGEqApo.dll
endlocal
