FROM rust:1.78

WORKDIR /usr/src/cicd
COPY . .

RUN cargo install --path .

CMD ["cicd"]
