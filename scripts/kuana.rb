# Homebrew cask for Kuana: `brew install --cask berkayakin/tap/kuana`.
#
# The release workflow copies this file into github.com/berkayakin/homebrew-tap
# (Casks/kuana.rb) with each release's version and the SHA-256 of its macOS zip,
# so edit it here, not in the tap.
cask "kuana" do
  version "0.1.19"
  sha256 "38dbb0d21413fc59883fd4ad662932eee1bc704e9ee042ccb118a9f3f03cffd6"

  url "https://github.com/berkayakin/kuana-editor/releases/download/v#{version}/Kuana-#{version}-macos.zip"
  name "Kuana"
  desc "Fast GPU-rendered code editor"
  homepage "https://github.com/berkayakin/kuana-editor"

  livecheck do
    url :url
    strategy :github_latest
  end

  # Kuana updates itself (src/updater.rs); brew upgrade only catches up a copy that hasn't.
  auto_updates true
  depends_on :macos

  app "Kuana.app"
  # The `kuana` command as Kuana's own Install Command writes it (src/cli.rs):
  # paths go to the running app, `kuana extension …` runs in the terminal.
  binary "kuana.wrapper.sh", target: "kuana"

  preflight_steps do
    write_file "kuana.wrapper.sh", <<~EOS
      #!/bin/sh
      [ "$1" = extension ] && exec '{{appdir}}/Kuana.app/Contents/MacOS/kuana' "$@"
      exec open -a '{{appdir}}/Kuana.app' "$@"
    EOS
    set_permissions "kuana.wrapper.sh", "0755"
  end

  zap trash: [
    "~/.config/kuana",
    "~/Library/Caches/dev.kuana.editor",
    "~/Library/HTTPStorages/dev.kuana.editor",
    "~/Library/Preferences/dev.kuana.editor.plist",
    "~/Library/Saved Application State/dev.kuana.editor.savedState",
  ]

  # Until releases are notarized (Developer ID secrets in release.yml), Gatekeeper
  # stops the first launch. Remove this once they are.
  caveats <<~EOS
    Kuana isn't notarized yet, so macOS blocks its first launch. Either open it once
    through System Settings → Privacy & Security → Open Anyway, or run:
      xattr -dr com.apple.quarantine "#{appdir}/Kuana.app"
  EOS
end
