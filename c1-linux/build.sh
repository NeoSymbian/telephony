#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# build.sh -- the telephony C1 reference image (docs/telephony.md C1): mainline Linux (postmarketOS
# linux-postmarketos-qcom-sdm670) with the modem userland, all in a RAM disk, as a fastboot boot image for the
# Pixel 3a XL (bonito). It never mounts or writes the eMMC: rmtfs serves the modem's EFS from RAM copies of the C0
# backup, and keeps the modem's writes in RAM (-r).
#
#   sh build.sh RADIO_BACKUP_DIR OUT_DIR
#     RADIO_BACKUP_DIR: modemst1.img, modemst2.img, fsg.img, fsc.img from `phone.py dump` (C0)
#     OUT_DIR:          gets c1-boot.img. It holds the phone's EFS (IMEI): keep it private.
#
# Needs Docker on an arm64 host (Apple Silicon runs the alpine:edge container natively).
# On the phone: fastboot boot c1-boot.img. USB gives an ACM shell and ECM network 172.16.42.1 (ssh root, no password;
# on the Mac set the "C1 Linux reference" interface to 172.16.42.2/24). `reboot-mode bootloader` goes back to fastboot.
set -eu
BACKUP=$(cd "$1" && pwd)
OUT=$(mkdir -p "$2" && cd "$2" && pwd)
HERE=$(cd "$(dirname "$0")" && pwd)
NAME=c1-build-$$
PMOS_KEY_SHA256=b70e6ffc64a652d63aee0c28cb9ecb2ca3b49d759ca7a023bcb854d08780964b	# pmbootstrap 2.1.0 (PyPI)
DTB=sdm670-google-bonito-sdc.dtb	# the user's phone has the SDC panel
CMDLINE="loglevel=7 qcom_q6v5_mss.dyndbg=+p qcom_glink_smem.dyndbg=+p qcom_smp2p.dyndbg=+p qrtr.dyndbg=+p"

docker run -d --name "$NAME" alpine:edge sleep 3600 >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1' EXIT
docker cp "$HERE/overlay/." "$NAME:/overlay/"
docker exec "$NAME" mkdir -p /efs
for p in modemst1:modem_fs1 modemst2:modem_fs2 fsg:modem_fsg fsc:modem_fsc; do
	docker cp "$BACKUP/${p%%:*}.img" "$NAME:/efs/${p##*:}"
done

docker exec -e KEYSHA="$PMOS_KEY_SHA256" -e DTB="$DTB" -e CMDLINE="$CMDLINE" "$NAME" sh -eu -c '
echo https://mirror.postmarketos.org/postmarketos/master >> /etc/apk/repositories
# bootstrap the postmarketOS key, then check it against the copy pinned above before trusting it
apk update -q --allow-untrusted 2>/dev/null
apk fetch -q --allow-untrusted -o /tmp postmarketos-keys
mkdir /tmp/k && tar -xzf /tmp/postmarketos-keys-*.apk -C /tmp/k 2>/dev/null || true
key=$(find /tmp/k -name build.postmarketos.org.rsa.pub | head -1)
echo "$KEYSHA  $key" | sha256sum -c -
cp "$key" /etc/apk/keys/
apk update -q
apk add -q mkbootimg cpio

R=/rootfs
mkdir -p $R/etc/apk && cp -r /etc/apk/keys /etc/apk/repositories $R/etc/apk/
# --no-scripts: the kernel and base packages would run initramfs and user hooks meant for a full install
apk add -q --root $R --initdb --no-scripts alpine-baselayout alpine-baselayout-data busybox busybox-extras \
	musl-utils eudev dbus polkit modemmanager libqmi qmi-utils qrtr rmtfs tqftpserv dropbear alsa-utils \
	alsa-ucm-conf alsa-ucm-conf-qcom-sdm670 kmod iproute2 strace q6voiced reboot-mode \
	linux-postmarketos-qcom-sdm670 firmware-google-sargo
cd $R
chroot $R /bin/busybox --install -s
# what the skipped scripts would have done: /lib links (modules, firmware), the module index, users
ln -s ../usr/lib/modules lib/modules
rmdir lib/firmware 2>/dev/null || true; ln -s ../usr/lib/firmware lib/firmware
chroot $R depmod -a "$(ls usr/lib/modules)"
grep -q "^root:" etc/passwd || sed -i "1i root:x:0:0:root:/root:/bin/sh" etc/passwd
grep -q "^root:" etc/group || sed -i "1i root:x:0:root" etc/group
grep -q "^root:" etc/shadow && sed -i "s/^root:[^:]*:/root::/" etc/shadow || echo "root::0:::::" >> etc/shadow
echo "messagebus:x:100:101:messagebus:/run/dbus:/sbin/nologin" >> etc/passwd; echo "messagebus:x:101:messagebus" >> etc/group
echo "polkitd:x:102:102:polkitd:/var/lib/polkit:/sbin/nologin" >> etc/passwd; echo "polkitd:x:102:" >> etc/group
mkdir -p root var/lib/polkit var/lib/rmtfs etc/polkit-1/rules.d
echo "polkit.addRule(function(a, s) { if (s.user == \"root\") return polkit.Result.YES; });" > etc/polkit-1/rules.d/00-c1-root.rules
cp /efs/* var/lib/rmtfs/
rm -f usr/lib/firmware/qcom/sdm670/sargo/venus.mbn.zst usr/lib/firmware/qcom/sdm670/sargo/a615_zap.mbn.zst
cp -a /overlay/. .
mv boot /boot-pkg
find . | cpio -o -H newc 2>/dev/null | gzip -6 > /initramfs.gz
cat /boot-pkg/vmlinuz /boot-pkg/dtbs/qcom/$DTB > /kernel-dtb
mkbootimg-osm0sis --header_version 0 --pagesize 4096 --base 0 --kernel_offset 0x8000 --ramdisk_offset 0x1000000 \
	--tags_offset 0x100 --kernel /kernel-dtb --ramdisk /initramfs.gz --cmdline "$CMDLINE" -o /c1-boot.img
'
docker cp "$NAME:/c1-boot.img" "$OUT/c1-boot.img"
ls -l "$OUT/c1-boot.img"
