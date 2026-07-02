#!/usr/bin/env bash
set -euo pipefail

bin_name="${BIN_NAME:-openai-codex-proxy}"
dist_dir="${DIST_DIR:-dist}"
appimagetool_version="${APPIMAGETOOL_VERSION:-12}"

case "$(uname -m)" in
  x86_64)
    target="${TARGET:-x86_64-unknown-linux-gnu}"
    appimage_arch="${APPIMAGE_ARCH:-x86_64}"
    asset_suffix="${ASSET_SUFFIX:-linux-x64}"
    ;;
  aarch64 | arm64)
    target="${TARGET:-aarch64-unknown-linux-gnu}"
    appimage_arch="${APPIMAGE_ARCH:-aarch64}"
    asset_suffix="${ASSET_SUFFIX:-linux-arm64}"
    ;;
  *)
    echo "Unsupported local AppImage architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

package_version="$(
  sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1
)"
if [[ -z "${package_version}" ]]; then
  echo "Could not resolve package version from Cargo.toml" >&2
  exit 1
fi

timestamp="$(date -u +%Y%m%d%H%M%S)"
short_sha="$(git rev-parse --short=12 HEAD 2>/dev/null || echo unknown)"
dirty=""
if ! git diff --quiet --ignore-submodules -- 2>/dev/null || \
   ! git diff --cached --quiet --ignore-submodules -- 2>/dev/null; then
  dirty="-dirty"
fi

asset_version="${ASSET_VERSION:-${package_version}-local.${timestamp}}"
build_tag="${CODEX_PROXY_BUILD_TAG:-local-${short_sha}${dirty}}"
asset="${bin_name}-${asset_version}-${asset_suffix}"
appdir="${dist_dir}/${asset}.AppDir"
appimage="${dist_dir}/${asset}.AppImage"
appimagetool="${APPIMAGETOOL:-${dist_dir}/appimagetool-${appimage_arch}.AppImage}"

echo "Building ${bin_name} for ${target} with build tag ${build_tag}"
CODEX_PROXY_BUILD_TAG="${build_tag}" cargo build --locked --release --target "${target}"

if [[ -e "${appdir}" || -e "${appimage}" ]]; then
  echo "Refusing to overwrite existing local AppImage output: ${asset}" >&2
  exit 1
fi

install -d \
  "${appdir}/usr/bin" \
  "${appdir}/usr/share/applications" \
  "${appdir}/usr/share/doc/${bin_name}" \
  "${appdir}/usr/share/icons/hicolor"

install -m 0755 "target/${target}/release/${bin_name}" "${appdir}/usr/bin/${bin_name}"
install -m 0644 README.md LICENSE "${appdir}/usr/share/doc/${bin_name}/"

cat > "${appdir}/AppRun" <<'APP_RUN'
#!/usr/bin/env sh
set -eu
appdir="$(dirname "$(readlink -f "$0")")"
if [ "$#" -eq 0 ]; then
  exec "${appdir}/usr/bin/openai-codex-proxy" tray
fi
exec "${appdir}/usr/bin/openai-codex-proxy" "$@"
APP_RUN
chmod +x "${appdir}/AppRun"

cat > "${appdir}/${bin_name}.desktop" <<DESKTOP_ENTRY
[Desktop Entry]
Type=Application
Name=OpenAI Codex Proxy
Comment=Local OpenAI-compatible proxy for the ChatGPT/Codex backend
Exec=${bin_name} tray
Icon=${bin_name}
Categories=Development;Network;
Terminal=false
DESKTOP_ENTRY
install -m 0644 "${appdir}/${bin_name}.desktop" \
  "${appdir}/usr/share/applications/${bin_name}.desktop"
install -m 0644 "assets/icons/hicolor/256x256/apps/${bin_name}.png" \
  "${appdir}/${bin_name}.png"
cp -R assets/icons/hicolor/* "${appdir}/usr/share/icons/hicolor/"

if [[ ! -x "${appimagetool}" ]]; then
  echo "Downloading appimagetool ${appimagetool_version} for ${appimage_arch}"
  curl -fsSL \
    -o "${appimagetool}" \
    "https://github.com/AppImage/AppImageKit/releases/download/${appimagetool_version}/appimagetool-${appimage_arch}.AppImage"
  chmod +x "${appimagetool}"
fi

ARCH="${appimage_arch}" APPIMAGE_EXTRACT_AND_RUN=1 \
  "${appimagetool}" "${appdir}" "${appimage}"
chmod +x "${appimage}"
sha256sum "${appimage}" > "${appimage}.sha256"

echo
echo "Built local AppImage:"
echo "  ${appimage}"
echo "Checksum:"
echo "  ${appimage}.sha256"
echo
echo "Run it with:"
echo "  ${appimage}"
