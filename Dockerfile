FROM rust:1.74

WORKDIR /usr/src/cicd
COPY . .

RUN cargo install --path .

CMD ["cicd"]
