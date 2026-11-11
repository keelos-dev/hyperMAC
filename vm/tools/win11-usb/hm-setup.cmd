@echo off
rem hyperMAC: WinPE runs this instead of Windows Setup (winpeshl.ini in boot.wim).
rem Virtualization.framework restarts the VM inside itself, so the host would never see setup's
rem reboot: setup runs with /noreboot, then this shuts down. The host (vm/src/winsetup.rs) puts
rem efiprobe in front of the installed boot manager, swaps the install stick for the drivers
rem stick and starts the VM again.
wpeinit
set HM=
for %%d in (C D E F G H I J K L M N O P Q R T U V W Y Z) do if exist %%d:\hypermac.cmd if exist %%d:\sources\boot.wim set HM=%%d
if "%HM%"=="" (
  echo hyperMAC: the install stick was not found
  cmd /k
  goto :eof
)
call %HM%:\hypermac.cmd %HM% pe
X:\sources\setup.exe /unattend:%HM%:\autounattend.xml /noreboot
wpeutil shutdown
