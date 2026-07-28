import json

# parsec 16-color ANSI palette (hex, no #)
P = {
  "background": "0A0E0C", "foreground": "D7FBE4",
  "cursor": "4AF626", "cursorText": "0A0E0C",
  "selectionBg": "163A22", "selectionFg": "EAFBF0",
  # 0..7 normal
  "black":"11342A".replace("11342A","0A0E0C"),  # keep true black bg-ish
  "red":"FF5C57","green":"4AF626","yellow":"FFB84D",
  "blue":"37AEE2","magenta":"FF5FD2","cyan":"4AD0E0","white":"C8D8CE",
  # 8..15 bright
  "brBlack":"3A4A40","brRed":"FF8079","brGreen":"7CFFB2","brYellow":"FFD08A",
  "brBlue":"6FD0F0","brMagenta":"FF93E2","brCyan":"86E9F5","brWhite":"EAFBF0",
}
ORDER = ["black","red","green","yellow","blue","magenta","cyan","white",
         "brBlack","brRed","brGreen","brYellow","brBlue","brMagenta","brCyan","brWhite"]

def rgbf(h):
    return [int(h[0:2],16)/255, int(h[2:4],16)/255, int(h[4:6],16)/255]

# --- 1. plain JSON reference ---
ref = {"name":"parsec","background":"#"+P["background"],"foreground":"#"+P["foreground"],
       "cursor":"#"+P["cursor"],"selection":"#"+P["selectionBg"],
       "ansi":{str(i):"#"+P[k] for i,k in enumerate(ORDER)}}
open("terminal-palette.json","w").write(json.dumps(ref,indent=2))

# --- 2. iTerm2 .itermcolors (plist) ---
def color_dict(h):
    r,g,b = rgbf(h)
    return (f"\t<dict>\n\t\t<key>Color Space</key>\n\t\t<string>sRGB</string>\n"
            f"\t\t<key>Red Component</key>\n\t\t<real>{r:.4f}</real>\n"
            f"\t\t<key>Green Component</key>\n\t\t<real>{g:.4f}</real>\n"
            f"\t\t<key>Blue Component</key>\n\t\t<real>{b:.4f}</real>\n"
            f"\t\t<key>Alpha Component</key>\n\t\t<real>1</real>\n\t</dict>\n")
parts = ['<?xml version="1.0" encoding="UTF-8"?>',
 '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">',
 '<plist version="1.0">','<dict>']
for i,k in enumerate(ORDER):
    parts.append(f"\t<key>Ansi {i} Color</key>")
    parts.append(color_dict(P[k]).rstrip("\n"))
for key,val in [("Background Color","background"),("Foreground Color","foreground"),
                ("Bold Color","foreground"),("Cursor Color","cursor"),
                ("Cursor Text Color","cursorText"),("Selection Color","selectionBg"),
                ("Selected Text Color","selectionFg"),("Link Color","cyan")]:
    parts.append(f"\t<key>{key}</key>")
    parts.append(color_dict(P[val]).rstrip("\n"))
parts.append("</dict>")
parts.append("</plist>")
open("parsec.itermcolors","w").write("\n".join(parts)+"\n")

# --- 3. Windows Terminal scheme fragment ---
wt = {"name":"parsec",
 "background":"#"+P["background"],"foreground":"#"+P["foreground"],
 "cursorColor":"#"+P["cursor"],"selectionBackground":"#"+P["selectionBg"],
 "black":"#"+P["black"],"red":"#"+P["red"],"green":"#"+P["green"],"yellow":"#"+P["yellow"],
 "blue":"#"+P["blue"],"purple":"#"+P["magenta"],"cyan":"#"+P["cyan"],"white":"#"+P["white"],
 "brightBlack":"#"+P["brBlack"],"brightRed":"#"+P["brRed"],"brightGreen":"#"+P["brGreen"],
 "brightYellow":"#"+P["brYellow"],"brightBlue":"#"+P["brBlue"],"brightPurple":"#"+P["brMagenta"],
 "brightCyan":"#"+P["brCyan"],"brightWhite":"#"+P["brWhite"]}
open("windows-terminal.json","w").write(json.dumps(wt,indent=2))

# --- 4. VS Code terminal customizations ---
vs = {"workbench.colorCustomizations":{
 "terminal.background":"#"+P["background"],"terminal.foreground":"#"+P["foreground"],
 "terminalCursor.foreground":"#"+P["cursor"],"terminal.selectionBackground":"#"+P["selectionBg"],
 "terminal.ansiBlack":"#"+P["black"],"terminal.ansiRed":"#"+P["red"],
 "terminal.ansiGreen":"#"+P["green"],"terminal.ansiYellow":"#"+P["yellow"],
 "terminal.ansiBlue":"#"+P["blue"],"terminal.ansiMagenta":"#"+P["magenta"],
 "terminal.ansiCyan":"#"+P["cyan"],"terminal.ansiWhite":"#"+P["white"],
 "terminal.ansiBrightBlack":"#"+P["brBlack"],"terminal.ansiBrightRed":"#"+P["brRed"],
 "terminal.ansiBrightGreen":"#"+P["brGreen"],"terminal.ansiBrightYellow":"#"+P["brYellow"],
 "terminal.ansiBrightBlue":"#"+P["brBlue"],"terminal.ansiBrightMagenta":"#"+P["brMagenta"],
 "terminal.ansiBrightCyan":"#"+P["brCyan"],"terminal.ansiBrightWhite":"#"+P["brWhite"]}}
open("vscode-terminal.json","w").write(json.dumps(vs,indent=2))

print("generated:", "terminal-palette.json parsec.itermcolors windows-terminal.json vscode-terminal.json")
