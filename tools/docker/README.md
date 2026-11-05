# Docker image for developing Eclipse

**English** · [Español](README.es.md)

Build the image (inherited from zCore; the base defaults to Ubuntu 20.04):

```
git clone --recursive https://github.com/Pryancito/eclipse.git
cd eclipse/tools/docker
OS_TYPE=ubuntu
OS_TAG=20.04
IMAGE_TAG=latest
./build_docker_image.sh ${OS_TYPE} ${OS_TAG} ${IMAGE_TAG}
```

Start a container:

```
IMAGE_NAME=zcore:${OS_TYPE}-${OS_TAG}-${IMAGE_TAG}
./start_container.sh ${IMAGE_NAME}
```
