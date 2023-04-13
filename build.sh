#!/bin/bash

# fail fast on any non-zero exits
set -e

# the docker image name and dockerhub repo
NAME="bnk-dogecoin-electrs"
REPO="boolnetwork"

VERSION=`head -n 3 ./Cargo.toml | egrep -o "([0-9\.]+)"`

echo $VERSION

echo "*** Building $NAME"
docker build -t $NAME -f Dockerfile .

docker login -u $REPO -p $DOCKER_PASS

echo "*** Tagging $REPO/$NAME"
if [[ $VERSION != *"beta"* ]]; then
  docker tag $NAME $REPO/$NAME:$VERSION
fi
docker tag $NAME $REPO/$NAME

echo "*** Publishing $NAME"
docker push $REPO/$NAME
