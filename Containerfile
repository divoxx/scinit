FROM rust:1.89

WORKDIR /usr/src/myapp
COPY . .

RUN cargo install --path .

ENTRYPOINT ["scinit", "--"]

