# Chapter 2: Setting up

> **In this chapter:** install the tools, start MongoDB, install
> mongo-relations, and check that everything talks to each other.
> Allow about 20 minutes, mostly waiting for downloads and compilation.

You will end up with three things running on your computer:

```
 terminal (curl) ──HTTP──▶ mongo-relations ──▶ MongoDB (in Docker)
    you type here          port 8080            port 27017
```

## Step 1: Install Docker

Docker runs MongoDB for you without a manual installation.

- **macOS / Windows:** install [Docker Desktop](https://www.docker.com/products/docker-desktop/) and start it.
- **Linux:** install [Docker Engine](https://docs.docker.com/engine/install/).

Check that it works:

```sh
docker --version
# Docker version 27.x.x, build ...
```

## Step 2: Install Rust

mongo-relations is written in Rust, and you'll build it from source.
Install Rust with [rustup](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Accept the defaults, then **open a new terminal** so the `cargo` command is
found. Check the version; it must be 1.85 or newer:

```sh
cargo --version
# cargo 1.9x.x (...)
```

If you already had Rust installed, update it with `rustup update stable`.

## Step 3: Start MongoDB

Run these two commands:

```sh
docker run -d --name mongo -p 27017:27017 mongo:7 --replSet rs0 --bind_ip_all
```

```sh
docker exec mongo mongosh --quiet --eval \
  'rs.initiate({_id: "rs0", members: [{_id: 0, host: "127.0.0.1:27017"}]})'
```

The first command downloads MongoDB 7 and starts it in the background,
reachable on port 27017. The second turns it into a **replica set**.

> 💡 **Behind the scenes:** a replica set is normally a group of MongoDB
> servers keeping copies of the same data. You're running a group of one.
> mongo-relations needs a replica set for two features you'll use later:
> **transactions** (several changes that succeed or fail together) and
> **change streams** (being notified when data changes).

Wait about five seconds, then check that MongoDB is ready:

```sh
docker exec mongo mongosh --quiet --eval 'db.hello().isWritablePrimary'
# true
```

If it prints `false`, wait a few more seconds and try again.

## Step 4: Install mongo-relations

Download the source code and install the server:

```sh
git clone https://github.com/AL-Hareth/mongo-relations
cd mongo-relations
cargo install --locked --path crates/mr-server
```

The first build takes a few minutes. When it finishes, the
`mongo-relations` command is available everywhere:

```sh
mongo-relations --version
# mongo-relations 0.1.0
```

## Step 5: Create a workspace and an API key

Make a folder for the tutorial's files and move into it:

```sh
mkdir ~/petclinic
cd ~/petclinic
```

mongo-relations only answers requests that carry an **API key**, a
password for programs. Choose one; this tutorial uses `tutorial-key`. The
server never stores the key itself, only a fingerprint of it (a *hash*).
Compute the fingerprint:

```sh
mongo-relations hash-key tutorial-key
# c6d2fdddfb2588009898ee7cb6d460a6fab1bec568029a8fe89fcdaeec60f745
```

## Step 6: Write the configuration file

Create a file called `config.toml` in `~/petclinic` with this content. Use
any text editor, or paste the whole block into your terminal:

```sh
cat > config.toml <<'EOF'
[mongodb]
uri = "mongodb://127.0.0.1:27017/?replicaSet=rs0"
database = "petclinic"

[server]
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8080"

[[auth.keys]]
name = "me"
sha256 = "c6d2fdddfb2588009898ee7cb6d460a6fab1bec568029a8fe89fcdaeec60f745"
role = "admin"
EOF
```

Line by line:

- `[mongodb]`: where MongoDB is (`uri`) and which database to use. The
  `petclinic` database doesn't exist yet; it is created automatically.
- `[server]`: the addresses mongo-relations listens on. You'll use the HTTP
  one (port 8080).
- `[[auth.keys]]`: your API key's fingerprint. `role = "admin"` allows
  everything, including changing the schema. (The other roles are `read`
  and `read_write`.)

Ask the server to check the file:

```sh
mongo-relations --config config.toml check-config
# configuration OK
```

## Step 7: Start the server

```sh
mongo-relations --config config.toml
```

You should see:

```
INFO mongo_relations: connected mongodb=7.0.45 transactions=true schema_version=0
INFO mongo_relations: listening grpc=127.0.0.1:50051 http=127.0.0.1:8080
```

**Leave this terminal open.** The server runs until you press `Ctrl+C`.
Open a **second terminal** for the rest of the tutorial, and move into your
workspace there too:

```sh
cd ~/petclinic
```

## Step 8: Say hello

The health endpoint needs no API key:

```sh
curl -s localhost:8080/health
```

```json
{"mongodb":{"reachable":true,"transactions":true,"version":"7.0.45"},"schema_version":0,"status":"ok","version":"0.1.0"}
```

- `"status":"ok"`: the server is up and can reach MongoDB.
- `"transactions":true`: the replica set from step 3 works.
- `"schema_version":0`: you haven't described any collections yet. That's
  the next chapter.

🎉 Everything is running.

## Next time you sit down

Your setup survives a reboot, but the programs need restarting:

```sh
docker start mongo                                  # start MongoDB again
cd ~/petclinic && mongo-relations --config config.toml   # start the server
```

Your data is kept between sessions. To wipe it and start the tutorial over,
run `docker exec mongo mongosh --quiet --eval 'db.getSiblingDB("petclinic").dropDatabase()'`.

## If something went wrong

| Problem | Fix |
|---|---|
| `docker: command not found` / `Cannot connect to the Docker daemon` | Docker isn't installed or isn't running. Start Docker Desktop. |
| `docker run` says the name `mongo` is already in use | You've done this before. Run `docker start mongo` instead. |
| `cargo: command not found` | Open a new terminal after installing Rust. |
| Build fails with an old Rust version | Run `rustup update stable`. |
| Server says `connecting to MongoDB` failed | MongoDB isn't running (`docker start mongo`), or step 3's `rs.initiate` wasn't run. |
| `/health` shows `"transactions":false` | The replica set wasn't initiated. Run the `rs.initiate` command from step 3. |
| `Address already in use` | Something else uses port 8080 or 50051. Change the ports in `config.toml`. |

---

← [Chapter 1](01-why-relations.md) · Next: [Chapter 3: Your first collection →](03-first-collection.md)
