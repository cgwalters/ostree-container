# ostree-container

This repository will contain code to map between [ostree](https://github.com/ostreedev/ostree) and OCI/Docker container image formats.

See ostree issue 2251 (TODO linkify later, don't want to spam GH link notifications)

These generated containers should work in 3 distinct ways:

- Be runnable directly in via Kubernetes or `podman run` - this should give you a usable shell the same way as a regular container
- Contain a small webserver (`ENTRYPOINT /ostree/repo-webserver`), which just serves the repository (and in particular, this may contain static deltas)
- Be able to be fetched by the client code in this repository and directly extracted into a target OSTree repository (i.e. support in-place OS updates)

# Filesystem layout

```
.
├── etc
├── ostree
│   ├── object-map      # (database allowing mapping e.g. /usr/bin/bash to an ostree checksum)
│   └── repo            # An archive mode repo that may contain e.g. static deltas
│   └── repo-webserver  # A binary usable as an entrypoint to dynamically serve an archive mode repo as well as deltas
└── usr
    ├── bin
    └── lib64
```

## Bundle an OSTree repository into a container

Given an OSTree repository, running *outside* a container:

```
$ ostree-container build --repo=/path/to/repo --ref=exampleos/x86_64/stable --oci-dir=/output/exampleos
```

`--oci-dir` creates an [OpenContainers image](https://github.com/opencontainers/image-spec/blob/master/spec.md) layout.

You can then e.g.

```
$ skopeo copy oci:/output/exampleos containers-storage:localhost/exampleos
$ podman run --rm -ti --entrypoint bash localhost/exampleos
```

Another option is `--push quay.io/exampleos/exampleos:stable` which would push directly to a registry.  This would particularly be intended to be usable inside a fully unprivileged container, just mounting in the secrets necessary to push to the target registry.

## Take an arbitrary container and convert it to be OSTree ready

There's nothing conceptually stopping us from having tooling that takes
an arbitrary container image and just makes it "ostree ready".  Or even
just dyanamically accepting a container image that has a kernel client side.

This *may* be in scope at some point in the future.

### ostree-containers and derivation

For an ostree-based OS that is derived from Fedora, 
`ostree-container build --from=registry.fedoraproject.org/fedora:33` would cause the generated container image to derive from the parent; in particular we de-duplicate content in the ostree commit from the base.

This would work equally well for a Debian+ostree OS to do `--from=docker.io/debian:stable`.

(In fact we may *require* this; TBD)

## Running an ostree-container as a webserver

It also works to run the ostree-container as a webserver, which will expose a webserver that responds to `GET /repo`.

The effect will be as if it was built from a `Dockerfile` that contains `EXPOSE 8080`; it will work to e.g.
`kubectl run nginx --image=quay.io/exampleos/exampleos:latest --replicas=1`
and then also create a service for it.

## Pulling an ostree-container directly

A primary goal of this effort is to make it fully native to an ostree-based operating system to pull a container image directly too.

This project will hence provide a CLI tool and a Rust library which speaks the Docker/OCI protocols enough to directly pull the container image, extracting it into the system `/ostree/repo` repository.

An important aspect of this is that the system will validate the GPG signature of the target OSTree commit, as well as validating the sha256 of the contained objects.

```
$ ostree-container pull --repo=/ostree/repo --ref=exampleos/x86_64/stable quay.io/exampleos/exampleos:stable
```

A project like rpm-ostree could hence support:

```
$ rpm-ostree rebase quay.io/exampleos/exampleos:stable
```
(Along with the usual `rpm-ostree upgrade` knowing to pull that container image)

### Integrating with future container deltas

See https://blogs.gnome.org/alexl/2020/05/13/putting-container-updates-on-a-diet/
