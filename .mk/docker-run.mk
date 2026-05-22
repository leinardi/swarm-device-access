# Local snippet (NOT part of make-common): runs the device-mapping-manager
# container locally with the host bind mounts and namespaces it needs to
# manipulate the host's cgroup v2 / BPF state.
#
# The daemon requires:
#   - the Docker UNIX socket (to subscribe to events and inspect containers)
#   - the host /sys tree at /host/sys (to write into /sys/fs/cgroup)
#   - host cgroup namespace, host pid namespace, --privileged
#
# Override DOCKER_RUN_ARGS to pass extra daemon flags:
#   make docker-run DOCKER_RUN_ARGS="-log-level debug -log-time"

DOCKER_SOCKET ?= /var/run/docker.sock
DOCKER_RUN_ARGS ?= -log-level info -log-time

.PHONY: docker-run
docker-run: ## Run the image locally with host cgroup/pid namespaces and required bind mounts
	docker run --rm \
		--name "$(IMAGE_NAME)" \
		--privileged \
		--cgroupns=host \
		--pid=host \
		-v "$(DOCKER_SOCKET):/var/run/docker.sock" \
		-v /sys:/host/sys \
		"$(IMAGE_REPO):$(IMAGE_TAG)" \
		$(DOCKER_RUN_ARGS)
