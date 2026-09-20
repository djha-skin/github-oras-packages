# github-oras-packages Design Document

This document summarizes what we're building and why.


## Why we aren't just using a real auto-index server

However, autoindex servers have serious limitations which this tool is intended
to address.

## Our Target Audience

Most people who want to host packages
these days are not operators with some experience with BASH. They're
developers. They built a thing and they want to host the packages in some
easily consumable form like a package repository. If it's broken, they can
debug it, but they want the debugging experience to be straightforward and
simple.

## The Previous Solution

The autoindex server shines here. It's easy to inspect what routes are
available to my package repository by simply perusing the files. It is usually
easy to upload to real autoindex servers using rsync or WebDAV or ftp or
whatever. Because of this, most package manager tools are built to work with
autoindex servers. In particular, dnf (rpm), apt (deb), pacman, and pypi are
designed to work with it. It makes for a relatively simple hosting experience.

## The Problems Our Users Have

### Authentication

However, autoindex servers have some real shortcomings. Nginx and apache's
authentication flows are, in a word, primitive. No OAuth, no bearer token
system that is really secure and scales to many users. Just HTTP Basic and
usually a single password for the whole server. Not ideal in a public internet
setting like the one to which those who use GitHub Registry are accustomed.

### Hosting

Then there's hosting in general. Most folks simply don't have an HTTP server
lying around. Getting SSL to work used to be impossible, but even now with
ACME/LetsEncrypt it's a bit of a pain. Users need hosting, but they don't
currently have a good option for this.

## Base functionality: Autoindex Server

What I want is, essentially, a proxy which, when pointed at a github packages
registry, exposes the same HTTP API and also HTTP web pages as a "normal"
autoindex server, like nginx or apache in autoindex mode. I want to have the
usual autoindex html pages I can use to view the files, and I also want paths
and routes to work more or less the same as that.

It's okay if the files shown are a subset of the objects in the registry; this
is expected, since likely most objects will simply be docker images. For
example, it's okay if the design dictates that all "files" the autoindex server
exposes have some tag or annotation demonstrating that they should show up in
the autoindex server.

The motivation for this design is simple. All of these tools -- dnf, apt,
pacman and pypi (see here ->
packaging.python.org/en/latest/guides/hosting-your-own-index/ ) are old enough
that they were built to work with autoindexed HTTP servers. The idea was to
allow operators to just host their stuff on some simple server if they wanted
to.

This base assumption in the design means developers will easily be able to debug
issues. It also allows for flexibility: If I create my own package manager (and
I [have](https://degasolv.readthedocs.io/en/master/why-degasolv.html),
I can just put its packages in an autoindex-like server like I usually do. Most
tools know how to do this.

I also want `oras push` and `oras pull` to work on our system, since that tool
will likely be used by any AI developers use to debug any problems. However,
they shouldn't need to download that tool to use the proxy either. They just
just be able to download our tool and "turn it on".

To that end, the tool needs to be able to terminate SSL. If configured with a
hostname, our tool should "just do this" using the LetsEncrypt service.

Finally, authentication and hosting (storage) need addressed. These two reasons
are the main reasons I have chosen to target GitHub Packages as the back-end.
It is an extremely popular tool that most developers know how to work with, and
it neatly solves the problemsof storage and authentication. This means the user
can run this on a raspberry pi at home that doesn't have any significant
storage attached. User management, if necessary, can simply be done in GitHub
itself, while public repos are also workable through GitHub.

## Additional Functionality

With the idea in mind that our users aren't "operators", they're not going to
want to just work with an autoindex server. They're going to want to have some
commands in a README somewhere that they can feed their AI that can add or
remove individual packages into the system "just do it" style. To that end, we
need subsystems for specific package managers like pypi or dnf to manage where
packages are in the system. Common operations like "add a package", "add a
package _version_", update and delete those things need to be added in
addition to the normal "read" functionality already provided by the
autoindesx/ORAS-compatible server subsystem. Thus, "read" operations will
always be through the autoindexed system, while different package manager
specific operations will be C U and D from CRUD. There will also be C U and D
provided by the auto index system, just in case the user prefers the old
autoindex tools for repo management (say for apt) but more especially in case
the user is using some custom package manager we do not support.


## MVP and demonstration

The tool will be considered "done" in stages, when the following core
capabilities have bene demonstrated:

* Autoindex HTTP browsing
* ORAS push/pull using oras cli
* LetsEncrypt out-of-the-box functionality
* Create, Update and Delete using CLI autoindex subcommand
* The server itself will use the `serve` command. It may write files out on
  first run or update them, but these should be like, SSL certs or whatever.
  Nothing significant should be stored on the actual server.
* Not only must it work with fixtures, but it must be tested against the actual
  GitHub Packages system, including user access, private repositories, and
  bearer token auth or whatever GHP natively uses.

That is MVP autoindex. The other milestones follow thus:

* Milestone 1: C/U/D for **pypi**, assuming a `pypi` top-level "folder"
* Milestone 2: C/U/D for **dnf**, assuming a `dnf` top-level "folder" (configurable)

Similar milestones for the other package management systems, with each getting
its own demo and "end-of-sprint" evaluation.

## What I don't want

* No weird routes: With the exception of the annotation we use to say "This is
  an autoindexed thing", if the "file" in the autoindex server "folder" is named
  "a/b/c", then the route should have "a/b/c" in it, preferrably at the end of
  the route like all http autoindex servers have. No base64 anywhere, hard to
  reason about and hard to debug.

* No custom route maps: We don't need this. All that stuff is built into the
  package managers. In fact, a key end-to-end test that must be completed for
  the MVP to be considered complete is using "normal",
  native pypi tools or apt repo-building tools to build indexes for packages,
  and then uploading and using them with autoindex. All the package management
  subsystems should do is do this under the hood. It should just all use
  "normal" stuff but automate it so it isn't annoying to use, and it should do
  it without external dependencies.