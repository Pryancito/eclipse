# Imagen de Docker para desarrollar Eclipse

[English](README.md) · **Español**

Compilar la imagen (heredada de zCore; la base por defecto es Ubuntu 20.04):

```
git clone --recursive https://github.com/Pryancito/eclipse.git
cd eclipse/tools/docker
OS_TYPE=ubuntu
OS_TAG=20.04
IMAGE_TAG=latest
./build_docker_image.sh ${OS_TYPE} ${OS_TAG} ${IMAGE_TAG}
```

Arrancar un contenedor:

```
IMAGE_NAME=zcore:${OS_TYPE}-${OS_TAG}-${IMAGE_TAG}
./start_container.sh ${IMAGE_NAME}
```
