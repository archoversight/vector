#!/usr/bin/env bash
set -uo pipefail

# nats_integration_env.sh
#
# SUMMARY
#
#   Builds and pulls down the Vector NATS Integration test environment

set -x

while getopts a:t:e: flag
do
    case "${flag}" in
        a) ACTION=${OPTARG};;
        t) CONTAINER_TOOL=${OPTARG};;
        e) CONTAINER_ENCLOSURE=${OPTARG};;
        :)
         echo "ERROR: Option -$OPTARG requires an argument"          usage
         ;;
        *)
          echo "ERROR: Invalid option -$OPTARG"
          usage
          ;;
    esac
done
shift $((OPTIND-1))

# Check required switches exist
if [ -z "${ACTION}" ] || [ -z "${CONTAINER_TOOL}" ] || [ -z "${CONTAINER_ENCLOSURE}" ]; then
    usage
fi


ACTION="${action:-"stop"}"
CONTAINER_TOOL="${tool:-"podman"}"
CONTAINER_ENCLOSURE="${enclosure:-"pod"}"

#
# Functions
#

start_podman () {
	"${CONTAINER_TOOL}" "${CONTAINER_ENCLOSURE}" create --replace --name vector-test-integration-nats -p 4222:4222
	"${CONTAINER_TOOL}" run -d --"${CONTAINER_ENCLOSURE}"=vector-test-integration-nats  --name vector_nats \
	 nats
}

start_docker () {
	"${CONTAINER_TOOL}" "${CONTAINER_ENCLOSURE}" create vector-test-integration-nats
	"${CONTAINER_TOOL}" run -d --"${CONTAINER_ENCLOSURE}"=vector-test-integration-nats -p 4222:4222 --name vector_nats \
	 nats
}

stop_podman () {
	"${CONTAINER_TOOL}" rm --force vector_nats 2>/dev/null; true
	"${CONTAINER_TOOL}" "${CONTAINER_ENCLOSURE}" stop vector-test-integration-nats 2>/dev/null; true
	"${CONTAINER_TOOL}" "${CONTAINER_ENCLOSURE}" rm --force vector-test-integration-nats 2>/dev/null; true
}

stop_docker () {
	"${CONTAINER_TOOL}" rm --force vector_nats 2>/dev/null; true
	"${CONTAINER_TOOL}" "${CONTAINER_ENCLOSURE}" rm vector-test-integration-nats 2>/dev/null; true
}

echo "Running $ACTION action for NATS integration tests environment"

"${ACTION}"_"${CONTAINER_TOOL}"
