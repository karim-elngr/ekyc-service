FROM python:3.12-alpine
WORKDIR /mock
COPY server.py .
USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["python3", "-u", "server.py"]
