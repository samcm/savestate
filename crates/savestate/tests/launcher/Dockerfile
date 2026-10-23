# Stands in for the launcher in tests/controller.rs: `launcher` marks the VM ready
# the way the real launcher does, then runs until the test creates
# /run/savestate/crash, and exits 1 like a crashed VM.
FROM busybox:1.37@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
ENTRYPOINT ["/bin/sh", "-c", "[ \"$1\" = launcher ] || exit 2; touch /run/savestate/ready; until [ -e /run/savestate/crash ]; do sleep 1; done; exit 1", "sh"]
