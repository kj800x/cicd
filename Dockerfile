FROM rust:1.85

WORKDIR /usr/src/cicd
COPY . .

RUN cargo install --path .

CMD ["cicd"]
