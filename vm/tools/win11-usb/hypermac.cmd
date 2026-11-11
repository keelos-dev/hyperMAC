@echo off
rem hyperMAC: %1 = this drive letter, %2 = pe (Windows PE, before setup), os (specialize pass)
rem or done (first logon: the install is finished)
if /i "%2"=="pe" goto pe
if /i "%2"=="os" goto os
if /i "%2"=="done" goto done
goto :eof
:pe
for %%v in (TPM SecureBoot RAM CPU Storage) do reg add HKLM\SYSTEM\Setup\LabConfig /v Bypass%%vCheck /t REG_DWORD /d 1 /f
for /r %1:\drivers\ %%i in (*.inf) do drvload "%%i"
goto :eof
:os
pnputil /add-driver %1:\drivers\*.inf /subdirs /install > C:\Windows\Temp\hypermac-drivers.log 2>&1
goto :eof
:done
rem hyperMAC (vm/src/winsetup.rs) sees this mark once the VM has shut down, and finishes up
echo done> %1:\hypermac-done.txt
echo done> C:\hypermac-done.txt
shutdown /s /t 5
goto :eof
