import { invoke } from "@tauri-apps/api/core";

let greetInputEl: HTMLInputElement | null;
let greetMsgEl: HTMLElement | null;
let apiStatusEl: HTMLElement | null;
let apiResultEl: HTMLElement | null;

async function greet() {
  if (greetMsgEl && greetInputEl) {
    // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
    greetMsgEl.textContent = await invoke("greet", {
      name: greetInputEl.value,
    });
  }
}

async function callDemoPatch() {
  if (!apiStatusEl || !apiResultEl) return;
  apiStatusEl.textContent = "Calling local engine API...";
  apiResultEl.textContent = "";
  const baseUrl = (await invoke("engine_base_url")) as string;
  const r = await fetch(`${baseUrl}/api/ui/demo`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ selected: greetInputEl?.value || "none" }),
  });
  const json = await r.json();
  apiStatusEl.textContent = `Status: ${r.status}`;
  apiResultEl.textContent = JSON.stringify(json, null, 2);
}

window.addEventListener("DOMContentLoaded", () => {
  greetInputEl = document.querySelector("#greet-input");
  greetMsgEl = document.querySelector("#greet-msg");
  apiStatusEl = document.querySelector("#api-status");
  apiResultEl = document.querySelector("#api-result");
  document.querySelector("#greet-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    greet();
  });
  document.querySelector("#api-demo-btn")?.addEventListener("click", (e) => {
    e.preventDefault();
    void callDemoPatch();
  });
});
