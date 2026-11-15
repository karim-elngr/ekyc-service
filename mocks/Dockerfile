FROM python:3.12-alpine
WORKDIR /mock
COPY fixtures.py server.py interactive.py ./
RUN mkdir /data && chown 65534:65534 /data
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["python3", "-u", "server.py"]
