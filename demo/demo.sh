#!/bin/bash

THIS_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" >/dev/null 2>&1 && pwd )"
ROOT="${THIS_DIR}/../"
SERVER_DIR="${ROOT}/crates/pipeline_manager/"

set -e

export REDPANDA_BROKERS=localhost:9092

docker kill redpanda || true
docker rm redpanda || true
docker run --name redpanda -p 9092:9092 --rm -itd docker.redpanda.com/vectorized/redpanda:v22.3.11

${ROOT}/scripts/start_manager.sh ${THIS_DIR}/pipeline_data
${ROOT}/scripts/start_prometheus.sh ${THIS_DIR}/pipeline_data

${THIS_DIR}/create_demo_projects.sh
