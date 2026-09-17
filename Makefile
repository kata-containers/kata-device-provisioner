IMAGE ?= kata-device-provisioner
TAG   ?= latest
# Commit of this build, with -dirty when the tree has uncommitted changes.
GIT_SHA := $(shell git rev-parse --short HEAD)$(shell git diff-index --quiet HEAD -- || echo -dirty)

.PHONY: build image push clippy fmt test clean

build:
	cargo build --release

clippy:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt --check

# No GPU and no cluster required: sysfs is mocked with temp directories.
test:
	cargo test

# .git is not in the docker build context — hand the commit in explicitly.
image:
	docker build -t $(IMAGE):$(TAG) --build-arg GIT_SHA=$(GIT_SHA) .

push:
	docker push $(IMAGE):$(TAG)

clean:
	cargo clean
