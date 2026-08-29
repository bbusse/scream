# Maintainer: Björn Busse <bj.rn@baerlin.eu>
pkgname=scream
pkgver=0.1.0
pkgrel=0
pkgdesc="Wayland RTSP server using ext-image-copy-capture-v1 for screencopy"
url="https://github.com/bbusse/scream"
arch="all"
license="BSD-3-Clause"
depends="gst-moonshine"
makedepends="cargo rust gstreamer-dev gst-rtsp-server-dev gst-plugins-base-dev glib-dev wayland-dev wayland-protocols-dev"
# Built directly from this checkout (no source= fetch), so builddir points
# straight at $startdir. srcdir is redirected off to the side: abuild's
# default srcdir ($startdir/src) collides with - and gets wiped by abuild
# before build() runs - this project's own src/ directory.
srcdir="$startdir/.abuild-src"
builddir="$startdir"

build() {
	cd "$builddir"
	cargo build --release --locked
}

package() {
	cd "$builddir"
	install -Dm755 target/release/scream "$pkgdir"/usr/bin/scream
}
