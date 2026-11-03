// Loads the web client (D47): the WebAssembly module starts itself on #app.
import init from "./pkg/lazy_web.js";

init().catch((error) => {
  console.error(error);
  const app = document.getElementById("app");
  if (app) {
    app.innerHTML =
      '<div class="splash"><div class="brand">Lazy</div><p>Lazy could not start in this browser. Try a current version of Chrome, Edge, Firefox, or Safari.</p></div>';
  }
});
