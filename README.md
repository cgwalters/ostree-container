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


# ostree vs OCI/Docker

Looking at this, one might ask: why even have ostree?  Why not just have the operating system directly use something like the [containers/image](https://github.com/containers/image/) storage?

The first answer to this is that it's a goal of this project to "hide" ostree usage; it should feel "native" to ship and manage the operating system "as if" it was just running a container.

But, ostree has a *lot* of stuff built up around it and we can't just throw that away.

## Understanding kernels

ostree was designed from the start to manage bootable operating system trees - hence the name of the project.  For example, ostree understands bootloaders and kernels/initramfs images.  Container tools don't.

## Signing

ostree also quite early on gained an opinionated mechanism to sign images (commits) via GPG.  As of this time there are multiple competing mechanisms for container signing, and it is not widely deployed.
For running random containers from `docker.io`, it can be OK to just trust TLS or pin via `@sha256` - a whole idea of Docker is that containers are isolated and it should be reasonably safe to
at least try out random containers.  But for the *operating system* its integrity is paramount because it's ultimately trusted.

## Deduplication

ostree's hardlink store is designed around de-duplication.  Operating systems can get large and they are most natural as "base images" - which in the Docker container model
are duplicated on disk.  Of course storage systems like containers/image could learn to de-duplicate; but it would be a use case that *mostly* applied to just the operating system.

## Being able to remove all container images

In Kubernetes, the kubelet will prune the image storage periodically, removing images not backed by containers.  If we store the operating system itself as an image...well, we'd
need to do something like teach the container storage to have the concept of an image that is "pinned" because it's actually the booted filesystem.  Or create a "fake" container
representing the running operating system.

Other projects in this space ended up having an "early docker" distinct from 

## Independence of complexity of container storage

This stuff could be done - but the container storage and tooling is already quite complex, and introducing a special case like this would be treading into new ground.

Today for example, cri-o ships a `crio-wipe.service` which removes all container storage across major version upgrades.

ostree is a fairly simple format and has been 100% stable throughout its life so far.

## ostree format has per-file integrity

More on this here: https://ostreedev.github.io/ostree/related-projects/#docker

## Allow hiding ostree while not reinventing everything

So, again the goal here is: make it feel "native" to ship and manage the operating system "as if" it was just running a container without throwing away everything in ostree today.

