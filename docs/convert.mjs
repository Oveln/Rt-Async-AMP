import fs from "fs";
import path from "path";
import { chromium } from "playwright";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const assetsDir = path.join(__dirname, "assets");
const fontsDir = path.join(__dirname, "node_modules/@excalidraw/excalidraw/dist/prod/fonts");
const files = fs.readdirSync(assetsDir).filter(f => f.endsWith(".excalidraw"));

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1600, height: 1000 } });

page.route("**/*.woff2", async (route) => {
  const segs = new URL(route.request().url()).pathname.split("/");
  const fi = segs.indexOf("fonts");
  if (fi >= 0 && fi < segs.length - 2) {
    const local = path.join(fontsDir, segs[fi + 1], segs[fi + 2]);
    if (fs.existsSync(local)) {
      await route.fulfill({ contentType: "font/woff2", body: fs.readFileSync(local) });
      return;
    }
  }
  await route.continue();
});

const EXC_URL = "https://esm.sh/@excalidraw/excalidraw@0.18.1?deps=react@18.3.1,react-dom@18.3.1";

await page.setContent(`<!DOCTYPE html><html><head>
<style>*{margin:0;padding:0}html,body,#root{width:100%;height:100vh;overflow:hidden;background:white}</style>
</head><body><div id="root"></div>
<script type="module">
import React from "https://esm.sh/react@18.3.1";
import { createRoot } from "https://esm.sh/react-dom@18.3.1/client";
import { Excalidraw, exportToCanvas, exportToSvg } from "${EXC_URL}";
window.React = React;
window.createRoot = createRoot;
window.ExcalidrawComp = Excalidraw;
window.exportToCanvas = exportToCanvas;
window.exportToSvg = exportToSvg;
window._ready = true;
</script>
</body></html>`, { waitUntil: "load" });

await page.waitForFunction("window._ready === true", { timeout: 60000 });
console.log("Excalidraw 0.18.1 loaded");

for (const file of files) {
  const src = path.join(assetsDir, file);
  const dst = src.replace(".excalidraw", ".svg");
  const scene = JSON.parse(fs.readFileSync(src, "utf-8"));

  // 1) Render with React component to trigger font loading
  await page.evaluate((data) => {
    const root = document.getElementById("root");
    root.innerHTML = "";
    window.createRoot(root).render(
      window.React.createElement(window.ExcalidrawComp, {
        initialData: {
          elements: data.elements || [],
          appState: Object.assign({ scrollToContent: true }, data.appState || {}),
        },
        viewModeEnabled: true, zenModeEnabled: true, theme: "light",
      })
    );
  }, scene);

  await page.waitForTimeout(8000);
  await page.evaluate(() => document.fonts.ready);

  const svgStr = await page.evaluate(async (data) => {
    const svg = await window.exportToSvg({
      elements: data.elements,
      appState: { ...data.appState, exportBackground: true },
      files: data.files ?? {},
    });
    return svg.outerHTML;
  }, scene);

  fs.writeFileSync(dst, svgStr);
  console.log(`${file} → ${path.basename(dst)} ${Math.round(svgStr.length / 1024)}KB`);
}

await browser.close();
console.log("done");
