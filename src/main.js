const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// State
let presets = [];
let tunnelStates = {}; // key: preset.id, value: TunnelState (e.g. { status: "Inactive" })
let activeScreen = "main-screen";

// DOM Elements
let mainScreen, addScreen, editScreen;
let presetsContainer, globalStatusDot, globalStatusText, activeBadge, stopAllBtn, quitBtn;

// Add Form Elements
let addForm, addNameInput, addPortInput, addKeyPathInput, addBrowseBtn, addSubmitBtn, addBackBtn;

// Edit Form Elements
let editForm, editIdInput, editNameInput, editPortInput, editKeyPathInput, editBrowseBtn, editDeleteBtn, editSubmitBtn, editBackBtn;

// Initialize
window.addEventListener("DOMContentLoaded", () => {
  initElements();
  setupEventListeners();
  loadData();
  setupTauriEventListeners();
});

function initElements() {
  // Screens
  mainScreen = document.getElementById("main-screen");
  addScreen = document.getElementById("add-screen");
  editScreen = document.getElementById("edit-screen");

  // Main UI
  presetsContainer = document.getElementById("presets-list-container");
  globalStatusDot = document.getElementById("global-status-dot");
  globalStatusText = document.getElementById("global-status-text");
  activeBadge = document.getElementById("active-badge");
  stopAllBtn = document.getElementById("stop-all-btn");
  quitBtn = document.getElementById("quit-btn");

  // Add Screen
  addForm = document.getElementById("add-form");
  addNameInput = document.getElementById("add-name");
  addPortInput = document.getElementById("add-port");
  addKeyPathInput = document.getElementById("add-key-path");
  addBrowseBtn = document.getElementById("add-browse-btn");
  addSubmitBtn = document.getElementById("add-submit-btn");
  addBackBtn = document.getElementById("add-back-btn");

  // Edit Screen
  editForm = document.getElementById("edit-form");
  editIdInput = document.getElementById("edit-preset-id");
  editNameInput = document.getElementById("edit-name");
  editPortInput = document.getElementById("edit-port");
  editKeyPathInput = document.getElementById("edit-key-path");
  editBrowseBtn = document.getElementById("edit-browse-btn");
  editDeleteBtn = document.getElementById("edit-delete-btn");
  editSubmitBtn = document.getElementById("edit-submit-btn");
  editBackBtn = document.getElementById("edit-back-btn");
}

function setupEventListeners() {
  // Navigation
  document.getElementById("add-preset-btn").addEventListener("click", () => showScreen("add-screen"));
  addBackBtn.addEventListener("click", () => showScreen("main-screen"));
  editBackBtn.addEventListener("click", () => showScreen("main-screen"));

  // Add Preset File Dialog
  addBrowseBtn.addEventListener("click", async () => {
    try {
      const path = await invoke("select_key_file");
      if (path) {
        addKeyPathInput.value = path;
      }
    } catch (err) {
      console.error("Error picking file:", err);
    }
  });

  // Edit Preset File Dialog
  editBrowseBtn.addEventListener("click", async () => {
    try {
      const path = await invoke("select_key_file");
      if (path) {
        editKeyPathInput.value = path;
      }
    } catch (err) {
      console.error("Error picking file:", err);
    }
  });

  // Add Form Submission
  addForm.addEventListener("submit", async (e) => {
    e.preventDefault();
    const name = addNameInput.value.trim();
    const port = parseInt(addPortInput.value.trim(), 10);
    const keyPath = addKeyPathInput.value.trim();

    if (!name || isNaN(port)) return;

    const newPreset = {
      id: generateUUID(),
      name,
      port,
      sshKeyPath: keyPath || null,
      customSubdomain: null
    };

    presets.push(newPreset);
    await savePresets();

    // Reset form
    addNameInput.value = "";
    addPortInput.value = "";
    addKeyPathInput.value = "";
    addSubmitBtn.disabled = true;

    showScreen("main-screen");
    renderPresets();
  });

  // Add Form Validation
  const validateAddForm = () => {
    const name = addNameInput.value.trim();
    const port = addPortInput.value.trim();
    addSubmitBtn.disabled = !name || !port || isNaN(parseInt(port, 10));
  };
  addNameInput.addEventListener("input", validateAddForm);
  addPortInput.addEventListener("input", validateAddForm);

  // Edit Form Submission
  editForm.addEventListener("submit", async (e) => {
    e.preventDefault();
    const id = editIdInput.value;
    const name = editNameInput.value.trim();
    const port = parseInt(editPortInput.value.trim(), 10);
    const keyPath = editKeyPathInput.value.trim();

    if (!id || !name || isNaN(port)) return;

    const idx = presets.findIndex(p => p.id === id);
    if (idx !== -1) {
      const oldPreset = presets[idx];
      const updatedPreset = {
        ...oldPreset,
        name,
        port,
        sshKeyPath: keyPath || null
      };
      presets[idx] = updatedPreset;
      await savePresets();

      // If the tunnel is active or connecting, restart it to apply changes
      const state = tunnelStates[id];
      if (state && (state.status === "Active" || state.status === "Connecting")) {
        // Stop the old process
        await invoke("stop_tunnel_cmd", { presetId: id });
        // Start the new process
        await invoke("start_tunnel_cmd", { preset: updatedPreset });
      }

      showScreen("main-screen");
      renderPresets();
    }
  });

  // Edit Delete Preset
  editDeleteBtn.addEventListener("click", async () => {
    const id = editIdInput.value;
    if (!id) return;

    // Stop tunnel if running
    await invoke("stop_tunnel_cmd", { presetId: id });
    delete tunnelStates[id];

    // Remove preset
    presets = presets.filter(p => p.id !== id);
    await savePresets();

    showScreen("main-screen");
    renderPresets();
  });

  // Stop All Button
  stopAllBtn.addEventListener("click", async () => {
    stopAllBtn.disabled = true;
    try {
      await invoke("stop_all_tunnels_cmd");
      // Force status update
      Object.keys(tunnelStates).forEach(id => {
        tunnelStates[id] = { status: "Inactive" };
      });
      renderPresets();
      updateGlobalStatus();
    } catch (err) {
      console.error("Error stopping all tunnels:", err);
    }
  });

  // Quit Button
  quitBtn.addEventListener("click", async () => {
    try {
      await invoke("exit_app");
    } catch (err) {
      console.error("Error quitting app:", err);
    }
  });
}

async function loadData() {
  try {
    presets = await invoke("get_presets");
    const states = await invoke("get_tunnel_states");

    // Initialize tunnel states
    presets.forEach(p => {
      tunnelStates[p.id] = states[p.id] || { status: "Inactive" };
    });

    renderPresets();
    updateGlobalStatus();
  } catch (err) {
    console.error("Error loading data:", err);
  }
}

async function savePresets() {
  try {
    await invoke("save_presets", { presets });
  } catch (err) {
    console.error("Error saving presets:", err);
  }
}

function setupTauriEventListeners() {
  // Listen to asynchronous state updates from Rust
  listen("tunnel-state-changed", (event) => {
    const { id, state } = event.payload;
    console.log(`State update received for ${id}:`, state);
    tunnelStates[id] = state;

    renderPresets();
    updateGlobalStatus();
  });
}

// Render the Presets List
function renderPresets() {
  presetsContainer.innerHTML = "";

  if (presets.length === 0) {
    presetsContainer.innerHTML = `
      <div class="empty-state">
        Пресеты не настроены. Нажмите +, чтобы добавить.
      </div>
    `;
    return;
  }

  presets.forEach(preset => {
    const state = tunnelStates[preset.id] || { status: "Inactive" };
    const isChecked = state.status === "Active" || state.status === "Connecting";
    const isDisabled = false;


    const item = document.createElement("div");
    item.className = "preset-item";

    // Header row of preset item
    let headerHTML = `
      <div class="preset-row-top">
        <div class="preset-info">
          <span class="preset-name">${escapeHtml(preset.name)}</span>
          <span class="preset-port">localhost:${preset.port}</span>
        </div>
        <div class="preset-actions-right">
          <button class="edit-icon-btn" data-id="${preset.id}" title="Редактировать">
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
              <path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"></path>
              <path d="M18.5 2.5a2.121 2.121 0 1 1 3 3L12 15l-4 1 1-4z"></path>
            </svg>
          </button>
          <label class="switch">
            <input type="checkbox" data-id="${preset.id}" ${isChecked ? 'checked' : ''} ${isDisabled ? 'disabled' : ''} />
            <span class="slider"></span>
          </label>
        </div>
      </div>
    `;

    item.innerHTML = headerHTML;

    // Append extra details depending on state (URL or Error msg)
    if (state.status === "Active" && state.data) {
      const details = document.createElement("div");
      details.className = "preset-details";
      details.innerHTML = `
        <div class="url-container">
          <a class="url-text" href="${state.data}" target="_blank">${state.data}</a>
          <button class="copy-btn" data-url="${state.data}">Копировать</button>
        </div>
      `;
      item.appendChild(details);
    } else if (state.status === "Connecting") {
      const details = document.createElement("div");
      details.className = "preset-details";
      details.innerHTML = `
        <div class="connecting-log">
          <div class="connecting-spinner"></div>
          Подключение...
        </div>
      `;
      item.appendChild(details);
    } else if (state.status === "Error" && state.data) {
      const details = document.createElement("div");
      details.className = "preset-details";
      details.innerHTML = `
        <div class="error-log">${escapeHtml(state.data)}</div>
      `;
      item.appendChild(details);
    }

    presetsContainer.appendChild(item);
  });

  // Attach dynamic event listeners to elements inside presets

  // Toggle switches
  presetsContainer.querySelectorAll(".switch input").forEach(input => {
    input.addEventListener("change", async (e) => {
      const id = e.target.dataset.id;
      const preset = presets.find(p => p.id === id);
      if (!preset) return;

      if (e.target.checked) {
        // Toggle on: Start Tunnel
        tunnelStates[id] = { status: "Connecting" };
        renderPresets();
        updateGlobalStatus();
        try {
          await invoke("start_tunnel_cmd", { preset });
        } catch (err) {
          console.error("Error starting tunnel:", err);
          tunnelStates[id] = { status: "Error", data: err.toString() };
          renderPresets();
          updateGlobalStatus();
        }
      } else {
        // Toggle off: Stop Tunnel
        try {
          await invoke("stop_tunnel_cmd", { presetId: id });
          tunnelStates[id] = { status: "Inactive" };
          renderPresets();
          updateGlobalStatus();
        } catch (err) {
          console.error("Error stopping tunnel:", err);
        }
      }
    });
  });

  // Edit buttons
  presetsContainer.querySelectorAll(".edit-icon-btn").forEach(btn => {
    btn.addEventListener("click", (e) => {
      // Find the button (e.target might be the SVG or path)
      const targetBtn = e.currentTarget;
      const id = targetBtn.dataset.id;
      const preset = presets.find(p => p.id === id);
      if (preset) {
        showEditScreen(preset);
      }
    });
  });

  // Copy buttons
  presetsContainer.querySelectorAll(".copy-btn").forEach(btn => {
    btn.addEventListener("click", async (e) => {
      const url = e.target.dataset.url;
      try {
        await navigator.clipboard.writeText(url);
        e.target.textContent = "Скопировано!";
        e.target.style.backgroundColor = "var(--color-success-bg)";
        e.target.style.color = "var(--color-success)";
        e.target.style.borderColor = "var(--color-success-border)";

        setTimeout(() => {
          e.target.textContent = "Копировать";
          e.target.style.backgroundColor = "";
          e.target.style.color = "";
          e.target.style.borderColor = "";
        }, 1500);
      } catch (err) {
        console.error("Failed to copy clipboard:", err);
      }
    });
  });

  // Open external links in default browser
  presetsContainer.querySelectorAll(".url-text").forEach(link => {
    link.addEventListener("click", async (e) => {
      e.preventDefault();
      const url = e.target.getAttribute("href");
      if (url) {
        try {
          console.error("Trying to open URL:", url);
          await invoke("open_url", { url });
        } catch (err) {
          console.error("Failed to open URL:", err);
        }
      }
    });
  });
}

// Global Status Bar Updater
function tournament() { } // helper
function updateGlobalStatus() {
  const states = Object.values(tunnelStates);

  const hasActive = states.some(s => s.status === "Active");
  const hasConnecting = states.some(s => s.status === "Connecting");
  const activeCount = states.filter(s => s.status === "Active").count || states.filter(s => s.status === "Active").length;

  // Update Status Indicator
  globalStatusDot.className = "status-dot";
  if (hasActive) {
    globalStatusDot.classList.add("active");
    globalStatusText.textContent = "Tunnelhunt запущен";
  } else if (hasConnecting) {
    globalStatusDot.classList.add("connecting");
    globalStatusText.textContent = "Подключение...";
  } else {
    globalStatusDot.classList.add("inactive");
    globalStatusText.textContent = "Туннели выключены";
  }

  // Update Active badge
  if (activeCount > 0) {
    activeBadge.textContent = activeCount;
    activeBadge.classList.remove("hidden");
    stopAllBtn.disabled = false;
  } else {
    activeBadge.classList.add("hidden");
    stopAllBtn.disabled = !hasConnecting; // keep stop all enabled if still connecting
  }
}

// Show/Hide Screens
function showScreen(screenId) {
  mainScreen.classList.add("hidden");
  addScreen.classList.add("hidden");
  editScreen.classList.add("hidden");

  document.getElementById(screenId).classList.remove("hidden");
  activeScreen = screenId;
}

function showEditScreen(preset) {
  editIdInput.value = preset.id;
  editNameInput.value = preset.name;
  editPortInput.value = preset.port;
  editKeyPathInput.value = preset.sshKeyPath || "";
  showScreen("edit-screen");
}

// UUID generator helper
function generateUUID() {
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, function (c) {
    var r = Math.random() * 16 | 0, v = c == 'x' ? r : (r & 0x3 | 0x8);
    return v.toString(16);
  });
}

// HTML escape helper to prevent XSS
function escapeHtml(str) {
  if (!str) return "";
  return str
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#039;");
}
