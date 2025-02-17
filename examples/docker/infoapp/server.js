// server.js
const http = require("http");
const os = require("os");
const { execSync } = require("child_process");
const url = require("url");
const crypto = require("crypto");

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

// CPU-intensive operation
function burnCPU(duration) {
  const start = Date.now();
  while (Date.now() - start < duration) {
    crypto.createHash("sha256").update(Math.random().toString()).digest("hex");
  }
}

const getBasicInfo = () => {
  const hostname = os.hostname();
  const networkInterfaces = os.networkInterfaces();
  const ipAddresses = Object.values(networkInterfaces)
    .flat()
    .filter((iface) => !iface.internal && iface.family === "IPv4")
    .map((iface) => iface.address);

  const dockerInfo = getDockerInfo();

  return `
    <h1>Container:</h1>
    <h2>Hostname: ${hostname}</h2>
    <h3>IP Addresses: ${ipAddresses.join(", ")}</h3>
    <p>Docker Container ID: ${dockerInfo.containerId || "N/A"}</p>
    <p>Docker Image Info: ${dockerInfo.imageId || "N/A"}</p>
  `;
};

const server = http.createServer(async (req, res) => {
  const parsedUrl = url.parse(req.url, true);

  if (req.method === "GET") {
    // CPU-intensive sleep endpoint
    if (parsedUrl.pathname === "/sleep") {
      const duration = parseInt(parsedUrl.query.duration) || 0;
      const startTime = Date.now();

      console.log(`CPU-intensive request received for ${duration}ms`);

      // Actually consume CPU while we wait
      burnCPU(duration);

      const actualDuration = Date.now() - startTime;
      res.writeHead(200, { "Content-Type": "text/plain" });
      res.end(`CPU burned for ${actualDuration}ms`);
      return;
    }

    if (parsedUrl.pathname === "/") {
      const size = parseInt(parsedUrl.query.size);

      if (!isNaN(size) && size > 0) {
        // For size-based testing, add the basic info at the start
        const basicInfo = getBasicInfo();

        // Optionally burn CPU while generating the response
        const cpuBurn = parseInt(parsedUrl.query.cpu) || 0;
        if (cpuBurn > 0) {
          console.log(
            `Burning CPU for ${cpuBurn}ms while generating large response`
          );
          burnCPU(cpuBurn);
        }

        const padding = Buffer.alloc(size, "x").toString();

        const html = `
          ${basicInfo}
          <hr>
          <h3>Large Response Test:</h3>
          <p>Added ${size} bytes of data</p>
          ${cpuBurn > 0 ? `<p>CPU burned for ${cpuBurn}ms</p>` : ""}
          <pre>${padding}</pre>
        `;

        res.writeHead(200, {
          "Content-Type": "text/html",
          "Content-Length": Buffer.byteLength(html),
        });
        res.end(html);
        return;
      }

      // Original behavior for standard requests
      const html = getBasicInfo();
      res.writeHead(200, { "Content-Type": "text/html" });
      res.end(html);
      return;
    }

    if (parsedUrl.pathname === "/tc") {
      const action = parsedUrl.query.action || "add";
      const delay = parsedUrl.query.delay || "50";

      try {
        if (action === "add") {
          // First remove any existing rules
          try {
            execSync("tc qdisc del dev eth0 root");
          } catch (e) {
            // Ignore error if no rules exist
          }

          // Add new rules
          execSync(
            `tc qdisc add dev eth0 root handle 1: tbf rate 1mbit burst 32kbit latency 400ms`
          );
          execSync(
            `tc qdisc add dev eth0 parent 1: handle 2: netem delay ${delay}ms 10ms distribution normal rate 1mbit limit 1000`
          );

          res.writeHead(200, { "Content-Type": "text/plain" });
          res.end(`Added traffic control rules to eth0`);
        } else if (action === "del") {
          execSync("tc qdisc del dev eth0 root");
          res.writeHead(200, { "Content-Type": "text/plain" });
          res.end("Removed traffic control from eth0");
        } else if (action === "show") {
          const output = execSync("tc qdisc show dev eth0").toString();
          res.writeHead(200, { "Content-Type": "text/plain" });
          res.end(output);
        }
      } catch (error) {
        res.writeHead(500, { "Content-Type": "text/plain" });
        res.end(`Error: ${error.message}`);
      }
      return;
    }

    // 404 for unknown endpoints
    res.writeHead(404, { "Content-Type": "text/plain" });
    res.end("Not Found");
  }
});

server.listen(PORT, () => {
  console.log(`Server running on port ${PORT}`);
});
