' Hidden launcher for smd (no console window).
'
' smd must run in the interactive desktop session: ConPTY children inherit its
' session, and only that session can host GUI windows. Register it at logon with
'   schtasks /create /tn smd /tr "wscript.exe \"<folder>\smd-launch.vbs\"" /sc onlogon /it /f
' or put a shortcut in shell:startup.
Dim sh, fso, exe
Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
exe = fso.BuildPath(fso.GetParentFolderName(WScript.ScriptFullName), "smd.exe")
sh.CurrentDirectory = sh.ExpandEnvironmentStrings("%USERPROFILE%")
sh.Run """" & exe & """", 0, False
