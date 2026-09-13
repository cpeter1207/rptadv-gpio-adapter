#!/bin/sh
## @file
## @brief Run a command in a disposable, exact-workspace labeled test container.
set -eu

if [ "$#" -lt 2 ]; then
	printf '%s\n' "usage: $0 IMAGE COMMAND [ARG...]" >&2
	exit 2
fi

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
image=$1
shift
scope=$(printf '%s' "$root" | cksum | awk '{print $1}')
project_label='org.rptadvanced.test.project=rptadv-gpio-adapter'
scope_label="org.rptadvanced.test.scope=$scope"
name="rptadv-gpio-adapter-test-$scope-$$"
pull_image=${RPTADV_CONTAINER_PULL:-1}

## @brief Remove only stale test containers belonging to this workspace.
cleanup_stale()
{
	stale=$(docker container ls --all --quiet --filter 'label=rpt_advanced.test=true' \
		--filter "label=$project_label" \
		--filter "label=$scope_label")
	[ -n "$stale" ] || return 0
	# Docker generated the identifiers through all three exact label filters.
	while IFS= read -r container; do
		[ -n "$container" ] || continue
		docker container rm --force "$container" >/dev/null
	done <<EOF
$stale
EOF
}

## @brief Remove this invocation's container on every exit path.
cleanup_current()
{
	docker container rm --force "$name" >/dev/null 2>&1 || true
}

cleanup_stale
trap cleanup_current EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "$pull_image" = '1' ]; then
	pull_output=$(docker image pull "$image" 2>&1)
	printf '%s\n' "$pull_output" >&2
	pulled_digest=$(printf '%s\n' "$pull_output" | sed -n 's/^Digest: //p' | tail -n 1)
	if [ -z "$pulled_digest" ]; then
		printf '%s\n' "could not determine the freshly pulled digest for $image" >&2
		exit 1
	fi
	printf '%s\n' "Using $image@$pulled_digest" >&2
elif [ "$pull_image" = '0' ]; then
	image_id=$(docker image inspect --format '{{.Id}}' "$image")
	printf '%s\n' "Using existing local image $image ($image_id)" >&2
else
	printf '%s\n' 'RPTADV_CONTAINER_PULL must be 0 or 1' >&2
	exit 2
fi

host_root=$root
case $(uname -s) in
	MINGW*|MSYS*)
		# Docker Desktop needs a Windows mount path while MSYS must leave the
		# container-only /workspace argument untouched.
		host_root=$(cd "$root" && pwd -W)
		export MSYS_NO_PATHCONV=1
		;;
esac

docker run --rm --name "$name" --label rpt_advanced.test=true \
	--label "$project_label" \
	--label "$scope_label" --volume "$host_root:/workspace" --workdir /workspace \
	"$image" "$@"
