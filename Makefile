# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 默默的阿文
#
# Host-side build only. Device profile generation runs on the device:
#   adb push packaging/profile-gen.sh /data/local/tmp/
#   adb exec-out sh /data/local/tmp/profile-gen.sh > profiles/device.toml

CARGO ?= cargo
TARGET_DIR ?= /tmp/tempo-build

export CARGO_TARGET_DIR := $(TARGET_DIR)

.PHONY: all check build clean

all: check build

check:
	$(CARGO) test --workspace

build:
	$(CARGO) build --release --workspace

clean:
	rm -rf "$(TARGET_DIR)"
