' Hidden launcher for smw (no console window).
'
' smw only needs to run as the same Windows user as smd; it does not need the
' interactive desktop. The API token comes from the user environment variable
' SMW_TOKEN, so the secret never appears in this task's command line.
' Register it at logon with
'   $vbs = "<folder>\smw-launch.vbs"
'   schtasks /create /tn smw /tr ('wscript.exe "{0}"' -f $vbs) /sc onlogon /it /f
Dim sh, fso, exe
Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
exe = fso.BuildPath(fso.GetParentFolderName(WScript.ScriptFullName), "smw.exe")
sh.CurrentDirectory = sh.ExpandEnvironmentStrings("%USERPROFILE%")
sh.Run """" & exe & """", 0, False
