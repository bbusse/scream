# Builds scream as a signed Alpine package via abuild and the APKBUILD at the
# repo root. A build artifact, not a runnable image: it produces
# /apk/scream.apk. Consumers install it with `apk add scream.apk`, verified
# against keys/apk-releases.rsa.pub. The gstreamer dependencies are declared
# in APKBUILD and pulled in with it, the binary lands at /usr/bin/scream, so
# no LD_LIBRARY_PATH, GST_PLUGIN_PATH or launcher script is needed
#
# Signing needs the private key paired with keys/apk-releases.rsa.pub at
# build time, supplied as a secret and never baked into the image:
#   podman build --secret id=abuild_privkey,src=/path/to/apk-releases.rsa .
FROM alpine:edge
RUN apk update && apk add --no-cache alpine-sdk sudo
RUN adduser -D builder && addgroup builder abuild && \
	echo 'builder ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/builder
WORKDIR /workspace
COPY . .
RUN chown -R builder:builder /workspace

ARG PACKAGER="Björn Busse <bj.rn@baerlin.eu>"
RUN mkdir -p /home/builder/.abuild && \
	echo "PACKAGER_PRIVKEY=\"/home/builder/.abuild/apk-releases.rsa\"" > /home/builder/.abuild/abuild.conf && \
	echo "PACKAGER=\"${PACKAGER}\"" >> /home/builder/.abuild/abuild.conf && \
	chown -R builder:builder /home/builder/.abuild
COPY keys/apk-releases.rsa.pub /etc/apk/keys/apk-releases.rsa.pub
COPY keys/apk-releases.rsa.pub /home/builder/.abuild/apk-releases.rsa.pub
RUN chown builder:builder /home/builder/.abuild/apk-releases.rsa.pub

USER builder
RUN --mount=type=secret,id=abuild_privkey,target=/home/builder/.abuild/apk-releases.rsa,uid=1000,gid=1000,required=true \
	sh -c 'cd /workspace && abuild checksum && abuild -r'

USER root
RUN mkdir -p /apk && cp /home/builder/.local/share/abuild/*/scream-*.apk /apk/scream.apk
