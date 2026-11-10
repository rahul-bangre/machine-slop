#!/bin/sh
set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd -P)
DEBIAN_SNAPSHOT=20261103T000000Z
DEBIAN_SOURCES="deb https://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT trixie main
deb https://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT trixie-updates main
deb https://snapshot.debian.org/archive/debian-security/$DEBIAN_SNAPSHOT trixie-security main"
CHROMIUM_PIN=$ROOT/bench/browser/chromium.json
DENO_PINS=$ROOT/bench/env/deno-release.json
FIRECRACKER_VERSION=v1.17.0
KERNEL_PATH=firecracker-ci/v1.15/ARCH/vmlinux-6.1.155

die() {
  echo "image: $*" >&2
  exit 1
}

pinned() {
  python3 -c 'import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[key]
print(" ".join(value) if isinstance(value, list) else value)' "$@"
}

[ $# -eq 2 ] || die "usage: $0 <image dir> <agentd binary>"
OUT=$1
AGENTD=$2
[ -x "$AGENTD" ] || die "$AGENTD is not an executable"
CACHE=${SLOP_IMAGE_CACHE:-$HOME/.cache/machine-slop}

case $(uname -m) in
  aarch64)
    ARCH=aarch64
    FIRECRACKER_SHA256=e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256
    KERNEL_SHA256=e3544b10603acbf3db492cb52e000d22ba202cb4b63b9add027565683e11c591
    DEBIAN_CHROMIUM=chromium
    CHROMIUM_EXECUTABLE=/usr/lib/chromium/chromium
    ;;
  x86_64)
    ARCH=x86_64
    FIRECRACKER_SHA256=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558
    KERNEL_SHA256=e20e46d0c36c55c0d1014eb20576171b3f3d922260d9f792017aeff53af3d4f2
    CHROMIUM_URL=$(pinned "$CHROMIUM_PIN" platforms linux-x86_64 url)
    CHROMIUM_SHA256=$(pinned "$CHROMIUM_PIN" platforms linux-x86_64 sha256)
    CHROMIUM_EXECUTABLE=/opt/chromium/$(pinned "$CHROMIUM_PIN" platforms linux-x86_64 executable)
    ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac

DENO_VERSION=$(sed -n 's/^deno //p' "$ROOT/.tool-versions")
DENO_URL=$(pinned "$DENO_PINS" "$DENO_VERSION" "linux-$ARCH" url)
DENO_SHA256=$(pinned "$DENO_PINS" "$DENO_VERSION" "linux-$ARCH" sha256)
PACKAGES="build-essential ca-certificates curl git iproute2 nodejs procps python3 python3-pip python3-venv socat tini unzip
$(pinned "$CHROMIUM_PIN" debian_trixie_packages) ${DEBIAN_CHROMIUM:-}"

sha256() {
  sha256sum "$1" | cut -d' ' -f1
}

fetch() {
  url=$1
  want=$2
  dest=$3
  if [ -f "$dest" ] && [ "$(sha256 "$dest")" = "$want" ]; then
    return
  fi
  curl -fsSL -o "$dest.part" "$url"
  got=$(sha256 "$dest.part")
  [ "$got" = "$want" ] || die "$url has sha256 $got, want $want"
  mv "$dest.part" "$dest"
}

base_rootfs() {
  key=$(printf '%s\n' "$DEBIAN_SOURCES" "$PACKAGES" "$ARCH" | sha256sum | cut -c1-16)
  tarball="$CACHE/rootfs-$key.tar"
  if [ ! -f "$tarball" ]; then
    echo "image: building Debian base rootfs (snapshot $DEBIAN_SNAPSHOT)" >&2
    sudo mmdebstrap --quiet --variant=minbase --format=tar \
      --keyring=/usr/share/keyrings/debian-archive-keyring.gpg \
      --aptopt='Acquire::Check-Valid-Until "false"' \
      --include="$(echo $PACKAGES | tr ' ' ',')" \
      trixie "$tarball.part" "$DEBIAN_SOURCES"
    sudo chown "$(id -u):$(id -g)" "$tarball.part"
    mv "$tarball.part" "$tarball"
  fi
  echo "$tarball"
}

unzip_into_rootfs() {
  archive=$1
  directory=$2
  sudo install -m 0644 "$archive" "$staging/root/tmp/archive.zip"
  sudo chroot "$staging/root" unzip -q /tmp/archive.zip -d "$directory"
  sudo rm "$staging/root/tmp/archive.zip"
}

build_rootfs() {
  tarball=$1
  dest=$2
  staging=$(mktemp -d)
  trap 'sudo rm -rf "$staging"' EXIT
  mkdir "$staging/root"
  sudo tar -C "$staging/root" -xf "$tarball"
  sudo install -m 0755 "$AGENTD" "$staging/root/usr/local/bin/agentd"
  sudo install -m 0755 "$ROOT/vm/image/slop-init" "$staging/root/sbin/slop-init"
  unzip_into_rootfs "$CACHE/deno-$DENO_VERSION-$ARCH.zip" /usr/local/bin
  if [ -n "${CHROMIUM_URL:-}" ]; then
    unzip_into_rootfs "$CACHE/chromium-$CHROMIUM_SHA256.zip" /opt/chromium
  fi
  sudo ln -s "$CHROMIUM_EXECUTABLE" "$staging/root/usr/local/bin/chromium"
  sudo chroot "$staging/root" /usr/local/bin/deno --version >&2
  sudo chroot "$staging/root" /usr/local/bin/chromium --version >&2
  sudo mkdir -p "$staging/root/mnt/scratch" "$staging/root/mnt/root"
  used_mib=$(sudo du -sm "$staging/root" | cut -f1)
  truncate -s "$((used_mib * 12 / 10 + 256))M" "$staging/rootfs.ext4"
  sudo mkfs.ext4 -q -F -L slop-root -d "$staging/root" "$staging/rootfs.ext4"
  mv "$staging/rootfs.ext4" "$dest"
  sudo rm -rf "$staging"
  trap - EXIT
}

mkdir -p "$CACHE" "$OUT"

firecracker_tgz="$CACHE/firecracker-$FIRECRACKER_VERSION-$ARCH.tgz"
fetch "https://github.com/firecracker-microvm/firecracker/releases/download/$FIRECRACKER_VERSION/firecracker-$FIRECRACKER_VERSION-$ARCH.tgz" \
  "$FIRECRACKER_SHA256" "$firecracker_tgz"
fetch "http://spec.ccfc.min.s3.amazonaws.com/$(echo "$KERNEL_PATH" | sed "s/ARCH/$ARCH/")" \
  "$KERNEL_SHA256" "$OUT/vmlinux"
tar -xzf "$firecracker_tgz" -O "release-$FIRECRACKER_VERSION-$ARCH/firecracker-$FIRECRACKER_VERSION-$ARCH" > "$OUT/firecracker.part"
chmod 0755 "$OUT/firecracker.part"
mv "$OUT/firecracker.part" "$OUT/firecracker"

fetch "$DENO_URL" "$DENO_SHA256" "$CACHE/deno-$DENO_VERSION-$ARCH.zip"
if [ -n "${CHROMIUM_URL:-}" ]; then
  fetch "$CHROMIUM_URL" "$CHROMIUM_SHA256" "$CACHE/chromium-$CHROMIUM_SHA256.zip"
fi

tarball=$(base_rootfs)
stamp=$(printf '%s\n' "$tarball" "$(sha256 "$AGENTD")" "$(sha256 "$ROOT/vm/image/slop-init")" \
  "$DENO_SHA256" "${CHROMIUM_SHA256:-}" "$CHROMIUM_EXECUTABLE" | sha256sum | cut -d' ' -f1)
if [ -f "$OUT/rootfs.ext4" ] && [ "$(cat "$OUT/rootfs.stamp" 2>/dev/null)" = "$stamp" ]; then
  echo "image: rootfs is current" >&2
else
  build_rootfs "$tarball" "$OUT/rootfs.ext4.part"
  mv "$OUT/rootfs.ext4.part" "$OUT/rootfs.ext4"
  echo "$stamp" > "$OUT/rootfs.stamp"
  echo "image: built $OUT/rootfs.ext4" >&2
fi
