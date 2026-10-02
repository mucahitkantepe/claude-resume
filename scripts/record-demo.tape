# claude-resume demo. Run scripts/record-demo.sh, which builds the binary and the made-up
# sessions this tape expects in target/demo.
Output demo.gif

Set FontSize 15
Set Width 1200
Set Height 700
Set Theme "Catppuccin Mocha"
Set Padding 16
Set TypingSpeed 60ms
Set Shell zsh

Hide
Type "export PATH=$PWD/target/release:$PATH CLAUDE_CONFIG_DIR=$PWD/target/demo/.claude CLAUDE_RESUME_NO_SYNC=1 && clear"
Enter
Show

# Launch
Type "claude-resume"
Enter
Sleep 2s

# Browse sessions
Sleep 1s
Down
Sleep 0.3s
Down
Sleep 0.3s
Down
Sleep 0.3s
Down
Sleep 0.3s

# Fuzzy search — misspelled but still finds it
Sleep 0.5s
Type "karabsan"
Sleep 2.5s

# Navigate the match
Sleep 1.5s

# Clear and search again
Ctrl+W
Sleep 0.5s
Type "websocket"
Sleep 2.5s

# Scroll preview
Ctrl+D
Sleep 0.8s
Ctrl+U
Sleep 1s

# Clear and try another
Ctrl+W
Sleep 0.5s
Type "terraform"
Sleep 2s

# Navigate
Down
Sleep 0.4s
Down
Sleep 0.4s
Up
Sleep 1.5s

# Exit
Escape
Sleep 0.5s
