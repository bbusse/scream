# Maintainer: Björn Busse <bj.rn@baerlin.eu>
pkgname=scream
pkgver=0.1.0
pkgrel=0
pkgdesc="Wayland RTSP server using ext-image-copy-capture-v1 for screencopy"
url="https://github.com/bbusse/scream"
arch="all"
license="BSD-3-Clause"
# The canonical plugin names, not gst-moonshine directly: moonshine's
# gst-moonshine package provides= all of these, so on a moonshine system apk
# pulls that, and on plain Alpine it pulls the stock packages. Depending on
# gst-moonshine by name needs it in a repo abuild can resolve at build time,
# which the CI does not have yet.
depends="gst-plugins-base gst-plugins-good gst-plugins-ugly gst-rtsp-server"
makedepends="cargo rust gstreamer-dev gst-rtsp-server-dev gst-plugins-base-dev glib-dev wayland-dev wayland-protocols-dev"
# Built directly from this checkout (no source= fetch), so builddir points
# straight at $startdir. srcdir is redirected off to the side: abuild's
# default srcdir ($startdir/src) is this project's own src/ directory, which
# abuild would wipe before build() runs
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
