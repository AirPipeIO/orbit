FROM node:18-alpine

# Install iproute2 for tc (traffic control)
RUN apk add --no-cache iproute2

WORKDIR /app
COPY package.json server.js ./

EXPOSE 80

CMD ["node", "server.js"]