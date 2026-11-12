@echo off
rem hyperMAC: the agent that lets the Mac show this Windows' program windows as its own
rem (hmagent.cs beside this file says what it does). Run as administrator. It starts at every
rem logon from then on.
setlocal
set "D=%ProgramFiles%\hyperMAC"
if not exist "%D%" mkdir "%D%"
set "CSC="
for %%d in (FrameworkArm64 Framework64 Framework) do if not defined CSC if exist "%WINDIR%\Microsoft.NET\%%d\v4.0.30319\csc.exe" set "CSC=%WINDIR%\Microsoft.NET\%%d\v4.0.30319\csc.exe"
if not defined CSC (
  echo hyperMAC agent: this Windows has no C# compiler ^(.NET Framework 4^) > "%D%\install.log"
  exit /b 1
)
rem The host talks to the agent through a port of the machine's own (a virtio serial port,
rem \\.\hypermac.agent): its driver, when this disc has it. Without it: over the network.
if exist "%~dp0vioserial\vioser.inf" pnputil /add-driver "%~dp0vioserial\vioser.inf" /install > "%D%\driver.log" 2>&1
sc stop hyperMACAgentChannel >nul 2>&1
taskkill /im hmagent.exe /f >nul 2>&1
"%CSC%" /nologo /optimize /target:winexe /r:System.ServiceProcess.dll /out:"%D%\hmagent.exe" "%~dp0hmagent.cs" > "%D%\install.log" 2>&1
if errorlevel 1 exit /b 1
copy /y "%~dp0agent.token" "%D%\agent.token" >nul
rem the token is this machine's: its users read it (the agent runs as the one logged on), nobody changes it
icacls "%D%\agent.token" /inheritance:r /grant:r *S-1-5-32-544:F *S-1-5-18:F *S-1-5-32-545:R >nul 2>&1
netsh advfirewall firewall delete rule name="hyperMAC agent" >nul 2>&1
netsh advfirewall firewall add rule name="hyperMAC agent" dir=in action=allow protocol=TCP localport=47800 program="%D%\hmagent.exe" >nul
reg add HKLM\Software\Microsoft\Windows\CurrentVersion\Run /v hyperMACAgent /t REG_SZ /d "\"%D%\hmagent.exe\"" /f >nul
rem The port's other end, a service (as SYSTEM, from the start of Windows; hmagent.cs Relay):
rem Windows lets only administrators and the system open the port, and the agent is the
rem logged-on user's program.
sc create hyperMACAgentChannel binPath= "\"%D%\hmagent.exe\" relay" start= auto DisplayName= "hyperMAC agent channel" >> "%D%\driver.log" 2>&1
sc start hyperMACAgentChannel >> "%D%\driver.log" 2>&1
rem (nostart: called while Windows is being set up, before anyone is logged on)
rem Started through Explorer, so that it runs as the logged-on user does at every later logon,
rem not with this script's administrator rights.
if /i not "%1"=="nostart" explorer.exe "%D%\hmagent.exe"
echo installed >> "%D%\install.log"
endlocal
