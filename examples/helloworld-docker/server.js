// server.js
const http = require("http");
const os = require("os");
const { execSync } = require("child_process");

const PORT = 80;

const getDockerInfo = () => {
  try {
    const containerId = execSync(
      'cat /proc/self/cgroup | grep "docker" | sed "s/^.*\\///" | head -n 1'
    )
      .toString()
      .trim();
    const imageId = execSync("cat /etc/hosts").toString().trim();
    return { containerId, imageId };
  } catch (error) {
    return { error: "Not running in Docker" };
  }
};

const server = http.createServer((req, res) => {
  if (req.method === "GET" && req.url === "/") {
    const hostname = os.hostname();
    const networkInterfaces = os.networkInterfaces();
    const ipAddresses = Object.values(networkInterfaces)
      .flat()
      .filter((iface) => !iface.internal && iface.family === "IPv4")
      .map((iface) => iface.address);

    const dockerInfo = getDockerInfo();

    const html = `
            <h1>:</h1>
            <h2>Hostname: ${hostname}</h2>
            <h3>IP Addresses: ${ipAddresses.join(", ")}</h3>
            <p>Docker Container ID: ${dockerInfo.containerId || "N/A"}</p>
            <p>Docker Image Info: ${dockerInfo.imageId || "N/A"}</p>
        `;

    res.writeHead(200, { "Content-Type": "text/html" });
    res.end(html);
  }
});

server.listen(PORT, () => {
  console.log(`Server running on port ${PORT}`);
});
