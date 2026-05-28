# Local snippet (NOT part of make-common): runs the swarm-device-access
# container locally with the host bind mounts and namespaces it needs to
# manipulate the host's cgroup v2 / BPF state.
#
# The daemon requires:
#   - the Docker UNIX socket (to subscribe to events and inspect containers)
#   - the host /sys tree at /host/sys (to write into /sys/fs/cgroup)
#   - host /dev tree (to stat device major/minor numbers)
#   - host cgroup, pid, and user namespaces, --privileged
#
# Optional: mount the host DBus socket to enable systemctl daemon-reload
# re-apply. dhi.io/static has no /var/run→/run symlink, so the container-side
# path must be /var/run/dbus/system_bus_socket (not /run/dbus/...).
#
# Override DOCKER_RUN_ARGS to pass extra daemon flags:
#   make docker-run DOCKER_RUN_ARGS="-log-level debug -log-time"

DOCKER_SOCKET ?= /var/run/docker.sock
DBUS_SOCKET   ?= /run/dbus/system_bus_socket
DOCKER_RUN_ARGS ?= -log-level info -log-time

.PHONY: docker-run
docker-run: ## Run the image locally with host cgroup/pid namespaces and required bind mounts
	docker run --rm \
		--name "$(IMAGE_NAME)" \
		--privileged \
		--cgroupns=host \
		--pid=host \
		--userns=host \
		-v "$(DOCKER_SOCKET):/var/run/docker.sock" \
		-v /sys:/host/sys \
		-v /dev:/dev \
		$(if $(wildcard $(DBUS_SOCKET)),-v "$(DBUS_SOCKET):/var/run/dbus/system_bus_socket") \
		"$(IMAGE_REPO):$(IMAGE_TAG)" \
		$(DOCKER_RUN_ARGS)
