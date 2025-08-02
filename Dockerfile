FROM rust:1.88

WORKDIR /usr/src/cicd
COPY . .

RUN cargo install --path .

CMD ["cicd"]
