import { useState, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import {
  Monitor,
  Wifi,
  Shield,
  MousePointer,
  FileUp,
  CheckCircle2,
  AlertCircle,
  RefreshCw,
  Info,
  X,
  Settings,
  Copy,
  Activity,
  Layers,
  ArrowRightLeft,
  FileText,
  Trash2,
  Keyboard
} from "lucide-react";

interface LocalInfo {
  hostname: string;
  ip: string;
}

interface DiscoveredNode {
  hostname: string;
  ip: string;
  port: number;
}

interface KvmStatusUpdate {
  active: boolean;
  role: string; // "host", "client", "idle"
  target: string;
}

interface FileProgress {
  transferId: string;
  status: string; // "starting", "processing", "completed", "error", "cancelled"
  fileName: string;
  progress: number; // 0.0 to 1.0
  speed: number;    // MB/s
  error: string | null;
  sha256Matches: boolean | null;
}

interface NetworkInterface {
  name: string;
  ip: string;
  is_virtual: boolean;
}

const KEYS_LIST = [
  { name: "A", code: 1 }, { name: "B", code: 2 }, { name: "C", code: 3 }, { name: "D", code: 4 },
  { name: "E", code: 5 }, { name: "F", code: 6 }, { name: "G", code: 7 }, { name: "H", code: 8 },
  { name: "I", code: 9 }, { name: "J", code: 10 }, { name: "K", code: 11 }, { name: "L", code: 12 },
  { name: "M", code: 13 }, { name: "N", code: 14 }, { name: "O", code: 15 }, { name: "P", code: 16 },
  { name: "Q", code: 17 }, { name: "R", code: 18 }, { name: "S", code: 19 }, { name: "T", code: 20 },
  { name: "U", code: 21 }, { name: "V", code: 22 }, { name: "W", code: 23 }, { name: "X", code: 24 },
  { name: "Y", code: 25 }, { name: "Z", code: 26 },
  { name: "F1", code: 61 }, { name: "F2", code: 62 }, { name: "F3", code: 63 }, { name: "F4", code: 64 },
  { name: "F5", code: 65 }, { name: "F6", code: 66 }, { name: "F7", code: 67 }, { name: "F8", code: 68 },
  { name: "F9", code: 69 }, { name: "F10", code: 70 }, { name: "F11", code: 71 }, { name: "F12", code: 72 }
];

function App() {
  const [localInfo, setLocalInfo] = useState<LocalInfo>({ hostname: "Определение...", ip: "127.0.0.1" });
  const [interfaces, setInterfaces] = useState<NetworkInterface[]>([]);
  const [selectedInterface, setSelectedInterface] = useState<NetworkInterface | null>(null);
  const [nodes, setNodes] = useState<DiscoveredNode[]>([]);
  const [selectedNode, setSelectedNode] = useState<DiscoveredNode | null>(null);
  
  // Settings Dialog Ref
  const settingsDialogRef = useRef<HTMLDialogElement>(null);
  const [copied, setCopied] = useState(false);
  const [copiedLogs, setCopiedLogs] = useState(false);
  const [clearedLogs, setClearedLogs] = useState(false);

  // KVM Settings
  const [kvmEnabled, setKvmEnabled] = useState(false);
  const [borderDirection, setBorderDirection] = useState<1 | 0>(1); // 1 = Right, 0 = Left
  const [accessibilityGranted, setAccessibilityGranted] = useState(true);

  // KVM Configurable Hotkeys state
  const [hotkeyCtrl, setHotkeyCtrl] = useState<boolean>(() => {
    return localStorage.getItem("hotkeyCtrl") !== "false"; // defaults to true
  });
  const [hotkeyAlt, setHotkeyAlt] = useState<boolean>(() => {
    return localStorage.getItem("hotkeyAlt") !== "false"; // defaults to true
  });
  const [hotkeyShift, setHotkeyShift] = useState<boolean>(() => {
    return localStorage.getItem("hotkeyShift") === "true"; // defaults to false
  });
  const [hotkeyKeyCode, setHotkeyKeyCode] = useState<number>(() => {
    const saved = localStorage.getItem("hotkeyKeyCode");
    return saved ? parseInt(saved, 10) : 11; // defaults to 11 (KeyK)
  });

  // Sync hotkey with Rust backend whenever it changes
  useEffect(() => {
    localStorage.setItem("hotkeyCtrl", String(hotkeyCtrl));
    localStorage.setItem("hotkeyAlt", String(hotkeyAlt));
    localStorage.setItem("hotkeyShift", String(hotkeyShift));
    localStorage.setItem("hotkeyKeyCode", String(hotkeyKeyCode));

    invoke("set_kvm_hotkey", {
      ctrl: hotkeyCtrl,
      alt: hotkeyAlt,
      shift: hotkeyShift,
      keyCode: hotkeyKeyCode
    }).catch((err) => console.error("Failed to sync hotkey with backend:", err));
  }, [hotkeyCtrl, hotkeyAlt, hotkeyShift, hotkeyKeyCode]);

  const getHotkeyString = () => {
    const parts = [];
    if (hotkeyCtrl) parts.push("Ctrl");
    if (hotkeyAlt) parts.push("Alt");
    if (hotkeyShift) parts.push("Shift");
    const keyMatch = KEYS_LIST.find((k) => k.code === hotkeyKeyCode);
    if (keyMatch) {
      parts.push(keyMatch.name);
    } else {
      parts.push("?");
    }
    return parts.join(" + ");
  };

  // KVM Status
  const [kvmStatus, setKvmStatus] = useState<KvmStatusUpdate>({ active: false, role: "idle", target: "" });

  // File Transfer State
  const [fileProgress, setFileProgress] = useState<FileProgress | null>(null);
  const [isSending, setIsSending] = useState(false);

  // Active IP address helper (user-selected interface or fallback to system default)
  const activeIp = selectedInterface?.ip || localInfo.ip;

  const handleCopyLink = () => {
    navigator.clipboard.writeText(`http://${activeIp}:53203`);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  const handleDownloadLogs = async () => {
    try {
      const destPath = await save({
        filters: [{ name: "Log Files", extensions: ["log", "txt"] }],
        defaultPath: "deskbridge.log",
      });
      if (!destPath) return; // user cancelled

      const saved = await invoke<boolean>("save_log_file", { destPath });
      if (saved) {
        alert("Файл логов успешно сохранен!");
      }
    } catch (err: any) {
      alert("Не удалось сохранить логи: " + err.toString());
    }
  };

  const handleCopyLogs = async () => {
    try {
      const content = await invoke<string>("get_log_content");
      await navigator.clipboard.writeText(content);
      setCopiedLogs(true);
      setTimeout(() => setCopiedLogs(false), 2000);
    } catch (err: any) {
      alert("Не удалось скопировать логи: " + err.toString());
    }
  };

  const handleClearLogs = async () => {
    if (!window.confirm("Вы уверены, что хотите очистить файл логов?")) {
      return;
    }
    try {
      await invoke("clear_logs");
      setClearedLogs(true);
      setTimeout(() => setClearedLogs(false), 2000);
    } catch (err: any) {
      alert("Не удалось очистить логи: " + err.toString());
    }
  };
  
  // Fetch Local Machine Info and Discovered Peers
  const fetchLocalInfo = async () => {
    try {
      const info = await invoke<LocalInfo>("get_local_info");
      setLocalInfo(info);
      return info;
    } catch (e) {
      console.error("Failed to get local machine info", e);
      return null;
    }
  };

  const fetchInterfaces = async (defaultIp: string) => {
    try {
      const list = await invoke<NetworkInterface[]>("get_network_interfaces");
      setInterfaces(list);
      
      // Attempt to auto-select the interface matching the default local IP
      if (list.length > 0) {
        const active = list.find((it) => it.ip === defaultIp) || list.find((it) => !it.is_virtual) || list[0];
        setSelectedInterface(active);
      }
    } catch (e) {
      console.error("Failed to get network interfaces", e);
    }
  };

  const fetchDiscoveredNodes = async () => {
    try {
      const resolved = await invoke<DiscoveredNode[]>("get_discovered_nodes");
      setNodes(resolved);
    } catch (e) {
      console.error("Failed to get discovered nodes", e);
    }
  };

  useEffect(() => {
    fetchLocalInfo().then((info) => {
      if (info) {
        fetchInterfaces(info.ip);
      }
    });
    fetchDiscoveredNodes();

    // Set up Tauri Event Listeners
    const unlistenMdns = listen<DiscoveredNode>("node-resolved", (event) => {
      const node = event.payload;
      setNodes((prev) => {
        if (prev.some((n) => n.ip === node.ip && n.port === node.port)) {
          return prev;
        }
        return [...prev, node];
      });
    });

    const unlistenKvm = listen<KvmStatusUpdate>("kvm-status", (event) => {
      setKvmStatus(event.payload);
      if (event.payload.active) {
        setKvmEnabled(true);
      }
    });

    const unlistenFile = listen<FileProgress>("file-progress", (event) => {
      const update = event.payload;
      setFileProgress(update);
      if (update.status === "completed" || update.status === "error" || update.status === "cancelled") {
        setIsSending(false);
      } else {
        setIsSending(true);
      }
    });

    return () => {
      unlistenMdns.then((f) => f());
      unlistenKvm.then((f) => f());
      unlistenFile.then((f) => f());
    };
  }, []);

  // Handle auto-selecting the last controlled device from localStorage
  useEffect(() => {
    if (nodes.length > 0 && !selectedNode) {
      const lastIp = localStorage.getItem("lastSelectedNodeIp");
      if (lastIp) {
        const match = nodes.find((n) => n.ip === lastIp);
        if (match) {
          setSelectedNode(match);
        }
      }
    }
  }, [nodes, selectedNode]);

  // Persist selected node IP to localStorage
  const selectNodeAndSave = (node: DiscoveredNode | null) => {
    setSelectedNode(node);
    if (node) {
      localStorage.setItem("lastSelectedNodeIp", node.ip);
    } else {
      localStorage.removeItem("lastSelectedNodeIp");
    }
  };

  // Configure/Toggle KVM
  const handleKvmToggle = async (checked: boolean) => {
    if (!selectedNode && checked) {
      alert("Пожалуйста, выберите устройство на карте сети.");
      return;
    }

    const targetIp = selectedNode ? selectedNode.ip : "";
    // Border X boundary: X = 0 for Left, X = W - 1 for Right
    // We send resolution placeholder; Rust dynamically queries display size natively now.
    const borderX = borderDirection === 1 ? 1919 : 1;

    try {
      const res = await invoke<string>("configure_kvm", {
        enabled: checked,
        targetIp,
        screenW: 1920,
        screenH: 1080,
        borderX,
        direction: borderDirection,
      });

      if (res === "success") {
        setKvmEnabled(checked);
        setAccessibilityGranted(true);
      }
    } catch (err: any) {
      if (err === "accessibility_not_granted") {
        setAccessibilityGranted(false);
        setKvmEnabled(false);
        alert("Предупреждение: Требуются права универсального доступа (Accessibility) для перехвата мыши.");
      } else {
        alert("Ошибка настройки KVM: " + err);
      }
    }
  };

  // Force/Trigger Manual Remote Control
  const handleManualControl = async () => {
    if (!selectedNode) return;
    try {
      const res = await invoke<string>("trigger_manual_control");
      console.log("Manual KVM session initiated:", res);
    } catch (err: any) {
      if (err === "accessibility_not_granted") {
        setAccessibilityGranted(false);
        alert("Ошибка: нет прав Accessibility на macOS.");
      } else {
        alert("Не удалось запустить KVM: " + err);
      }
    }
  };

  const handleReleaseControl = async () => {
    try {
      await invoke("release_manual_control");
    } catch (err) {
      console.error(err);
    }
  };

  // Select and Stream File via P2P
  const handleSendFile = async () => {
    if (!selectedNode) {
      alert("Пожалуйста, сначала выберите устройство-получатель.");
      return;
    }

    try {
      const filePath = await open({ multiple: false, directory: false }) as string | null;
      if (!filePath) return; // user cancelled

      setIsSending(true);
      setFileProgress({
        transferId: "starting",
        status: "starting",
        fileName: filePath.replace(/\\/g, "/").split("/").pop() || "file",
        progress: 0.0,
        speed: 0.0,
        error: null,
        sha256Matches: null,
      });

      await invoke("send_file", {
        targetIp: selectedNode.ip,
        filePath,
      });
    } catch (err: any) {
      setIsSending(false);
      setFileProgress((prev) => prev ? {
        ...prev,
        status: "error",
        error: err.toString(),
      } : null);
      alert("Ошибка отправки файла: " + err);
    }
  };

  const handleCancelTransfer = async () => {
    try {
      await invoke("cancel_file_transfer");
    } catch (err) {
      console.error(err);
    }
  };

  // Dialog click-backdrop-to-close handler
  const handleDialogClick = (e: React.MouseEvent<HTMLDialogElement>) => {
    const dialog = settingsDialogRef.current;
    if (!dialog) return;
    const rect = dialog.getBoundingClientRect();
    const isInDialog = (
      rect.top <= e.clientY && e.clientY <= rect.top + rect.height &&
      rect.left <= e.clientX && e.clientX <= rect.left + rect.width
    );
    if (!isInDialog) {
      dialog.close();
    }
  };

  return (
    <div className="min-h-screen bg-[#07080b] text-neutral-100 flex flex-col font-sans select-none antialiased grid-mesh relative">
      {/* Background glow effects */}
      <div className="absolute top-[-10%] left-[-10%] w-[500px] h-[500px] rounded-full bg-indigo-600/5 blur-[120px] pointer-events-none z-0" />
      <div className="absolute bottom-[-10%] right-[-10%] w-[500px] h-[500px] rounded-full bg-purple-600/5 blur-[120px] pointer-events-none z-0" />

      {/* Header */}
      <header className="border-b border-neutral-900/60 bg-neutral-950/30 backdrop-blur-lg px-8 py-5 flex items-center justify-between sticky top-0 z-50">
        <div className="flex items-center gap-3">
          <div className="h-11 w-11 rounded-2xl bg-gradient-to-tr from-indigo-500 via-indigo-600 to-purple-600 flex items-center justify-center shadow-lg shadow-indigo-600/20 font-extrabold text-xl tracking-tight text-white pulse-glow-indigo">
            DB
          </div>
          <div>
            <h1 className="font-extrabold text-lg tracking-tight bg-gradient-to-r from-white via-neutral-100 to-neutral-400 bg-clip-text text-transparent">
              DeskBridge
            </h1>
            <p className="text-[10px] text-neutral-500 font-mono tracking-wider">LOCAL MESH NETWORK • v2.2.0</p>
          </div>
        </div>

        {/* Local Machine IP Card */}
        <div className="flex items-center gap-4">
          <div className="flex items-center gap-4 bg-neutral-900/40 border border-neutral-800/40 rounded-2xl px-5 py-2.5 text-xs backdrop-blur-md">
            <div className="relative flex h-2 w-2 shrink-0">
              <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-emerald-400 opacity-75"></span>
              <span className="relative inline-flex rounded-full h-2 w-2 bg-emerald-500"></span>
            </div>
            <div className="text-left">
              <div className="text-[9px] uppercase tracking-wider text-neutral-500 font-bold">Имя компьютера</div>
              <div className="font-bold text-neutral-200">{localInfo.hostname}</div>
            </div>
            <div className="h-6 w-[1px] bg-neutral-800" />
            <div className="text-left">
              <div className="text-[9px] uppercase tracking-wider text-neutral-500 font-bold">Активный IP</div>
              <div className="font-mono font-bold text-indigo-400">{activeIp}</div>
            </div>
          </div>
          
          <button 
            onClick={() => { fetchDiscoveredNodes(); fetchLocalInfo(); }}
            className="p-3 rounded-2xl border border-neutral-800/60 bg-neutral-900/30 hover:bg-neutral-900/80 transition-all hover:border-neutral-700 active-depress text-neutral-400 hover:text-white"
            title="Обновить карту сети"
          >
            <RefreshCw className="w-4 h-4" />
          </button>
          
          <button 
            onClick={() => settingsDialogRef.current?.showModal()}
            className="p-3 rounded-2xl border border-neutral-800/60 bg-neutral-900/30 hover:bg-neutral-900/80 transition-all hover:border-neutral-700 active-depress text-neutral-400 hover:text-white flex items-center gap-2 group"
          >
            <Settings className="w-4 h-4 group-hover:rotate-45 transition-transform duration-300" />
            <span className="text-xs font-bold tracking-wide">Настройки</span>
          </button>
        </div>
      </header>

      {/* Main Grid */}
      <main className="flex-1 max-w-7xl w-full mx-auto p-8 grid grid-cols-1 lg:grid-cols-12 gap-8 items-start relative z-10">
        
        {/* Left Section: Network Map (Span 7) */}
        <section className="lg:col-span-7 flex flex-col gap-6 h-full">
          <div className="glass-panel glass-panel-glow rounded-3xl p-6 flex flex-col flex-1 min-h-[460px]">
            <div className="flex justify-between items-start mb-6">
              <div>
                <h2 className="font-extrabold text-base text-neutral-100 flex items-center gap-2">
                  <Monitor className="w-4 h-4 text-indigo-400" />
                  Карта устройств
                </h2>
                <p className="text-xs text-neutral-500 mt-1">
                  Выберите компьютер из локальной сети для KVM-управления или отправки файлов.
                </p>
              </div>
              <span className="text-[10px] font-bold tracking-wider uppercase bg-neutral-900 border border-neutral-800 text-neutral-400 px-3.5 py-1.5 rounded-full font-mono">
                Подключено: {nodes.length}
              </span>
            </div>

            {/* Room Map Nodes Layout */}
            <div className="flex-1 grid grid-cols-2 sm:grid-cols-3 gap-5 items-center justify-center p-4">
              
              {/* Local PC Node */}
              <div className="border border-indigo-500/20 bg-indigo-500/5 rounded-3xl p-5 flex flex-col items-center justify-center text-center group cursor-default relative overflow-hidden transition-all duration-300">
                <div className="absolute inset-0 bg-gradient-to-b from-indigo-500/5 to-transparent pointer-events-none" />
                <div className="w-16 h-16 rounded-2xl bg-indigo-500/10 text-indigo-400 flex items-center justify-center mb-4 border border-indigo-500/20 pulse-glow-indigo">
                  <Monitor className="w-8 h-8" />
                </div>
                <div className="font-bold text-sm text-indigo-200 truncate max-w-full">
                  {localInfo.hostname}
                </div>
                <div className="text-[9px] uppercase tracking-wider text-indigo-400/80 font-bold font-mono mt-1.5">Это устройство</div>
              </div>

              {/* Discovered Nodes */}
              {nodes.map((node) => {
                const isSelected = selectedNode?.ip === node.ip;
                return (
                  <button
                    key={`${node.ip}:${node.port}`}
                    onClick={() => selectNodeAndSave(isSelected ? null : node)}
                    className={`border rounded-3xl p-5 flex flex-col items-center justify-center text-center transition-all duration-300 hover-lift active-depress relative overflow-hidden ${
                      isSelected
                        ? "border-purple-500/60 bg-purple-500/10 text-white shadow-lg shadow-purple-500/10"
                        : "border-neutral-800/80 bg-neutral-900/20 hover:border-neutral-700/80 text-neutral-300 hover:bg-neutral-900/40"
                    }`}
                  >
                    {isSelected && (
                      <div className="absolute inset-0 bg-gradient-to-b from-purple-500/5 to-transparent pointer-events-none" />
                    )}
                    <div className={`w-16 h-16 rounded-2xl flex items-center justify-center mb-4 border transition-all duration-300 ${
                      isSelected
                        ? "bg-purple-500/20 text-purple-400 border-purple-500/30 pulse-glow-purple"
                        : "bg-neutral-950/60 border-neutral-800 text-neutral-400 hover:text-white"
                    }`}>
                      <Monitor className="w-8 h-8" />
                    </div>
                    <div className="font-bold text-sm truncate max-w-full">
                      {node.hostname}
                    </div>
                    <div className="text-[10px] text-neutral-500 font-mono mt-1.5">{node.ip}</div>
                  </button>
                );
              })}

              {/* Discovery Loading State */}
              {nodes.length === 0 && (
                <div className="col-span-full border border-dashed border-neutral-800/40 rounded-3xl p-8 flex flex-col items-center justify-center text-center text-neutral-500 min-h-[180px]">
                  <div className="w-14 h-14 rounded-2xl bg-neutral-900/60 flex items-center justify-center mb-4 border border-neutral-800/40">
                    <Wifi className="w-6 h-6 text-neutral-600 animate-pulse" />
                  </div>
                  <div className="font-bold text-sm text-neutral-400">Поиск соседних устройств...</div>
                  <p className="text-[10px] text-neutral-500 max-w-xs mt-1.5 leading-relaxed">
                    Запустите DeskBridge на других ПК в этой же сети. Устройства обнаружатся автоматически.
                  </p>
                </div>
              )}
            </div>
            
            {/* Visual Screen Linkage Pill */}
            {selectedNode && (
              <div className="mt-4 p-4 border border-purple-500/10 bg-purple-500/5 rounded-2xl flex items-center justify-between text-xs animate-fade-in backdrop-blur-md">
                <span className="text-neutral-300 flex items-center gap-2.5">
                  <ArrowRightLeft className="w-4 h-4 text-purple-400 shrink-0" />
                  Выбрано устройство: <strong className="text-purple-300">{selectedNode.hostname}</strong> ({selectedNode.ip})
                </span>
                <button 
                  onClick={() => selectNodeAndSave(null)} 
                  className="text-neutral-500 hover:text-white font-bold text-[10px] uppercase hover:bg-neutral-800/50 px-3 py-1.5 rounded-xl transition"
                >
                  Отмена
                </button>
              </div>
            )}
          </div>
        </section>

        {/* Right Section: Control Panels (Span 5) */}
        <section className="lg:col-span-5 flex flex-col gap-6 w-full z-10">
          
          {/* Universal Control (KVM) Config */}
          <div className="glass-panel glass-panel-glow rounded-3xl p-6">
            <h2 className="font-extrabold text-base text-neutral-100 flex items-center gap-2 mb-4">
              <MousePointer className="w-4 h-4 text-indigo-400" />
              Виртуальный KVM (Universal Control)
            </h2>

            {/* macOS Security Alert */}
            {!accessibilityGranted && (
              <div className="mb-4 border border-rose-500/30 bg-rose-500/5 rounded-2xl p-4 flex gap-3 text-xs text-rose-300">
                <Shield className="w-5 h-5 text-rose-400 shrink-0 mt-0.5" />
                <div>
                  <h4 className="font-bold text-rose-200 mb-1">Необходимы права доступа</h4>
                  <p className="text-rose-400/90 leading-normal">
                    Для перехвата мыши на macOS предоставьте права приложению в: <strong>Настройки системы &gt; Конфиденциальность и безопасность &gt; Универсальный доступ</strong>.
                  </p>
                </div>
              </div>
            )}

            <div className="flex flex-col gap-4 text-xs">
              
              {/* Screen Position Direction */}
              <div className="flex items-center justify-between border-b border-neutral-900/60 py-3">
                <span className="text-neutral-400 font-medium">Расположение экрана соседа</span>
                <div className="flex bg-neutral-950 p-1 rounded-xl border border-neutral-900">
                  <button
                    onClick={() => { setBorderDirection(0); if (kvmEnabled) setTimeout(() => handleKvmToggle(true), 50); }}
                    className={`px-4 py-2 rounded-lg font-bold transition-all text-[11px] uppercase tracking-wider ${
                      borderDirection === 0
                        ? "bg-indigo-600 text-white shadow-md shadow-indigo-600/10"
                        : "text-neutral-500 hover:text-neutral-200"
                    }`}
                  >
                    Слева
                  </button>
                  <button
                    onClick={() => { setBorderDirection(1); if (kvmEnabled) setTimeout(() => handleKvmToggle(true), 50); }}
                    className={`px-4 py-2 rounded-lg font-bold transition-all text-[11px] uppercase tracking-wider ${
                      borderDirection === 1
                        ? "bg-indigo-600 text-white shadow-md shadow-indigo-600/10"
                        : "text-neutral-500 hover:text-neutral-200"
                    }`}
                  >
                    Справа
                  </button>
                </div>
              </div>

              {/* Master KVM Activation Switch */}
              <div className="flex items-center justify-between py-3">
                <div>
                  <span className="text-neutral-200 font-bold block">Свободный переход границы</span>
                  <span className="text-[10px] text-neutral-500 block mt-1 leading-relaxed">
                    Курсор мыши автоматически перейдет на соседний ПК при пересечении границы экрана.
                  </span>
                </div>
                <button
                  onClick={() => handleKvmToggle(!kvmEnabled)}
                  className={`w-13 h-7.5 rounded-full p-1 transition-colors duration-300 relative shrink-0 ${
                    kvmEnabled ? "bg-indigo-600" : "bg-neutral-800"
                  }`}
                >
                  <div className={`w-5.5 h-5.5 rounded-full bg-white transition-transform duration-300 shadow-md ${
                    kvmEnabled ? "translate-x-5.5" : "translate-x-0"
                  }`} />
                </button>
              </div>

              {/* KVM Active session controls */}
              {kvmEnabled && selectedNode && (
                <div className="mt-4 border border-indigo-500/20 bg-indigo-500/5 rounded-2xl p-4 flex flex-col gap-3.5 backdrop-blur-md">
                  <div className="flex justify-between items-center text-xs">
                    <span className="text-neutral-400">Статус KVM:</span>
                    <span className={`font-bold px-2.5 py-1 rounded-lg text-[9px] font-mono uppercase tracking-wider ${
                      kvmStatus.active
                        ? "bg-purple-500/20 text-purple-300 animate-pulse border border-purple-500/30"
                        : "bg-emerald-500/20 text-emerald-300 border border-emerald-500/30"
                    }`}>
                      {kvmStatus.active ? "Управление удаленное" : "Управление локальное"}
                    </span>
                  </div>

                  <div className="h-[1px] bg-neutral-900" />
                  
                  {/* Failsafe & Toggle Notice */}
                  <div className="flex flex-col gap-2 bg-neutral-950/30 p-3.5 rounded-2xl border border-neutral-900/60">
                    <div className="text-[10px] text-neutral-400 flex items-center gap-2">
                      <Keyboard className="w-3.5 h-3.5 text-indigo-400 shrink-0" />
                      <span>Горячая клавиша переключения: <strong className="px-1.5 py-0.5 rounded bg-neutral-900 border border-neutral-850 text-indigo-300 font-mono text-[9px] select-none uppercase tracking-wide">{getHotkeyString()}</strong></span>
                    </div>
                    <div className="text-[10px] text-neutral-500 flex items-center gap-2">
                      <Info className="w-3.5 h-3.5 text-neutral-600 shrink-0" />
                      <span>Экстренный сброс: <strong className="font-mono text-neutral-400 text-[9px]">Ctrl + Alt + Escape</strong></span>
                    </div>
                  </div>

                  <div className="flex gap-2">
                    {kvmStatus.active ? (
                      <button
                        onClick={handleReleaseControl}
                        className="flex-1 font-bold text-center border border-rose-500/30 hover:border-rose-500/80 bg-rose-500/10 hover:bg-rose-500/20 text-rose-300 rounded-xl py-2.5 transition active-depress text-[11px] uppercase tracking-wider"
                      >
                        Вернуть мышь домой
                      </button>
                    ) : (
                      <button
                        onClick={handleManualControl}
                        className="flex-1 font-bold text-center bg-gradient-to-r from-indigo-500 to-purple-600 hover:from-indigo-600 hover:to-purple-700 text-white rounded-xl py-2.5 transition active-depress shadow-lg shadow-indigo-600/15 text-[11px] uppercase tracking-wider"
                      >
                        Войти вручную
                      </button>
                    )}
                  </div>
                </div>
              )}
            </div>
          </div>

          {/* High-Speed P2P File Transfer Panel */}
          <div className="glass-panel glass-panel-glow rounded-3xl p-6">
            <h2 className="font-extrabold text-base text-neutral-100 flex items-center gap-2 mb-4">
              <FileUp className="w-4 h-4 text-purple-400" />
              P2P Передача файлов
            </h2>

            <div className="flex flex-col gap-4 text-xs">
              
              {/* Drag and Drop selector block */}
              <button
                disabled={isSending || !selectedNode}
                onClick={handleSendFile}
                className={`border border-dashed rounded-3xl p-6 transition-all duration-300 text-center flex flex-col items-center justify-center gap-3 relative overflow-hidden group ${
                  isSending
                    ? "border-neutral-800 bg-neutral-900/10 cursor-not-allowed text-neutral-600"
                    : !selectedNode
                    ? "border-neutral-800 bg-neutral-900/5 cursor-not-allowed text-neutral-500"
                    : "border-purple-500/20 hover:border-purple-500/50 bg-purple-500/5 hover:bg-purple-500/10 cursor-pointer text-neutral-300"
                }`}
              >
                <div className={`w-12 h-12 rounded-2xl flex items-center justify-center transition-colors duration-300 ${
                  !selectedNode 
                    ? "bg-neutral-950 text-neutral-600 border border-neutral-900"
                    : isSending
                    ? "bg-neutral-950 text-neutral-600 border border-neutral-900"
                    : "bg-purple-500/10 text-purple-400 border border-purple-500/20 group-hover:bg-purple-500/20 group-hover:text-purple-300 group-hover:scale-105"
                }`}>
                  <FileUp className="w-6 h-6" />
                </div>
                <div>
                  <div className="font-bold text-sm">Выбрать и отправить файл</div>
                  <p className="text-[10px] text-neutral-500 mt-1">
                    {!selectedNode 
                      ? "Сначала выберите устройство на карте"
                      : "Прямая высокоскоростная сеть P2P"}
                  </p>
                </div>
              </button>

              {/* Progress and Speed details */}
              {fileProgress && (
                <div className="border border-neutral-850 bg-neutral-950/40 rounded-2xl p-4 flex flex-col gap-3.5 animate-fade-in backdrop-blur-md">
                  <div className="flex justify-between items-start gap-3">
                    <div className="truncate max-w-[65%]">
                      <span className="font-bold text-neutral-200 block truncate text-xs">
                        {fileProgress.fileName}
                      </span>
                    </div>
                    <span className={`font-bold text-[9px] uppercase font-mono px-2 py-0.5 rounded-md border shrink-0 tracking-wider ${
                      fileProgress.status === "completed"
                        ? "bg-emerald-500/20 text-emerald-300 border-emerald-500/30"
                        : fileProgress.status === "error"
                        ? "bg-rose-500/20 text-rose-300 border-rose-500/30"
                        : fileProgress.status === "cancelled"
                        ? "bg-neutral-800 text-neutral-400 border-neutral-700"
                        : "bg-purple-500/20 text-purple-300 animate-pulse border-purple-500/30"
                    }`}>
                      {fileProgress.status === "completed" ? "Готово" :
                       fileProgress.status === "error" ? "Ошибка" :
                       fileProgress.status === "cancelled" ? "Отменено" :
                       fileProgress.status === "starting" ? "Запуск..." : "Передача"}
                    </span>
                  </div>

                  {/* Speed Visualizer */}
                  {fileProgress.status === "processing" && (
                    <div className="flex items-center gap-3 bg-neutral-900/30 border border-neutral-800/40 rounded-xl p-2.5 animate-pulse">
                      <Activity className="w-4 h-4 text-purple-400 shrink-0" />
                      <div className="text-[10px]">
                        <span className="text-neutral-500">Скорость: </span>
                        <strong className="text-purple-300 font-mono">{fileProgress.speed.toFixed(2)} MB/s</strong>
                      </div>
                    </div>
                  )}

                  {/* Progress bar */}
                  <div className="flex flex-col gap-1.5">
                    <div className="w-full bg-neutral-950 rounded-full h-2.5 overflow-hidden border border-neutral-900/60">
                      <div 
                        className={`h-full transition-all duration-150 rounded-full ${
                          fileProgress.status === "completed"
                            ? "bg-gradient-to-r from-emerald-500 to-teal-500 shadow-md shadow-emerald-500/10"
                            : fileProgress.status === "error"
                            ? "bg-rose-500"
                            : "bg-gradient-to-r from-indigo-500 to-purple-500"
                        }`}
                        style={{ width: `${fileProgress.progress * 100}%` }}
                      />
                    </div>
                    <div className="flex justify-between items-center text-[10px] text-neutral-500 font-mono">
                      <span>{(fileProgress.progress * 100).toFixed(0)}%</span>
                      <span>{fileProgress.status === "processing" ? "Передача..." : ""}</span>
                    </div>
                  </div>

                  {/* Integrity checksum check */}
                  {fileProgress.sha256Matches !== null && (
                    <div className={`flex items-start gap-2.5 p-3 rounded-xl text-[10px] border ${
                      fileProgress.sha256Matches
                        ? "bg-emerald-500/5 text-emerald-400 border-emerald-500/10"
                        : "bg-rose-500/5 text-rose-400 border-rose-500/10"
                    }`}>
                      {fileProgress.sha256Matches ? (
                        <>
                          <CheckCircle2 className="w-4.5 h-4.5 text-emerald-500 shrink-0 mt-0.5" />
                          <div>
                            <span className="font-bold block text-neutral-200">Целостность проверена</span>
                            <span className="text-[9px] text-neutral-400 block mt-0.5">Контрольная сумма SHA-256 совпадает. Файл идентичен оригиналу.</span>
                          </div>
                        </>
                      ) : (
                        <>
                          <AlertCircle className="w-4.5 h-4.5 text-rose-500 shrink-0 mt-0.5" />
                          <div>
                            <span className="font-bold block text-neutral-200">Контрольная сумма не совпадает</span>
                            <span className="text-[9px] text-neutral-400 block mt-0.5">Файл был поврежден при передаче. Пожалуйста, отправьте его повторно.</span>
                          </div>
                        </>
                      )}
                    </div>
                  )}

                  {fileProgress.error && (
                    <div className="flex items-center gap-2 p-3 bg-rose-500/5 border border-rose-500/10 rounded-xl text-[10px] text-rose-400">
                      <AlertCircle className="w-4 h-4 text-rose-500 shrink-0" />
                      <span>{fileProgress.error}</span>
                    </div>
                  )}

                  {isSending && (
                    <button
                      onClick={handleCancelTransfer}
                      className="font-bold text-center border border-neutral-800 hover:border-neutral-700 bg-neutral-900/60 hover:bg-neutral-900/90 text-neutral-400 hover:text-white rounded-xl py-2 transition active-depress text-[10px] uppercase tracking-wider"
                    >
                      Прервать
                    </button>
                  )}
                </div>
              )}
            </div>
          </div>
        </section>
      </main>

      {/* Settings Modal (Native Dialog) */}
      <dialog 
        ref={settingsDialogRef} 
        onClick={handleDialogClick}
        className="rounded-3xl p-0 w-full max-w-lg shadow-2xl glass-panel text-white animate-scale-in"
      >
        <div className="flex flex-col max-h-[85vh] relative overflow-hidden">
          {/* Header */}
          <div className="px-6 py-5 border-b border-neutral-900 flex items-center justify-between bg-neutral-950/20">
            <div className="flex items-center gap-2.5">
              <div className="w-9 h-9 rounded-xl bg-indigo-500/10 text-indigo-400 flex items-center justify-center border border-indigo-500/10">
                <Settings className="w-4.5 h-4.5" />
              </div>
              <h3 className="font-extrabold text-base text-neutral-100">Настройки DeskBridge</h3>
            </div>
            <button 
              onClick={() => settingsDialogRef.current?.close()}
              className="p-2 rounded-xl text-neutral-500 hover:text-white hover:bg-neutral-900/60 transition active-depress"
            >
              <X className="w-5 h-5" />
            </button>
          </div>

          {/* Scrollable Content */}
          <div className="p-6 flex flex-col gap-6 overflow-y-auto">
            
            {/* Section 1: Active Network Interface */}
            <div className="flex flex-col gap-3">
              <h4 className="text-[10px] font-extrabold text-indigo-400 uppercase tracking-wider flex items-center gap-1.5">
                <Layers className="w-3.5 h-3.5" /> Сетевой адаптер
              </h4>
              <p className="text-xs text-neutral-400 leading-relaxed">
                Выберите сетевой адаптер, через который работает DeskBridge. Это обновит адрес локальной сети и веб-портала.
              </p>
              <div className="bg-neutral-950/40 border border-neutral-900 rounded-2xl p-4">
                <select
                  value={selectedInterface?.name || ""}
                  onChange={(e) => {
                    const match = interfaces.find((it) => it.name === e.target.value);
                    if (match) setSelectedInterface(match);
                  }}
                  className="w-full bg-neutral-900 border border-neutral-800 rounded-xl px-4 py-2.5 text-xs text-neutral-200 outline-none hover:border-neutral-700 transition"
                >
                  {interfaces.map((it) => (
                    <option key={it.name} value={it.name}>
                      {it.name} ({it.ip}){it.is_virtual ? " [Виртуальный]" : ""}
                    </option>
                  ))}
                </select>
              </div>
            </div>

            {/* Section 2: KVM Hotkey Configuration */}
            <div className="flex flex-col gap-3 border-t border-neutral-900 pt-5">
              <h4 className="text-[10px] font-extrabold text-indigo-400 uppercase tracking-wider flex items-center gap-1.5">
                <Keyboard className="w-3.5 h-3.5" /> Горячие клавиши KVM
              </h4>
              <p className="text-xs text-neutral-400 leading-relaxed">
                Настройте клавиатурную комбинацию для мгновенного перехвата и возврата мыши и ввода.
              </p>
              <div className="bg-neutral-950/40 border border-neutral-900 rounded-2xl p-4 flex flex-col gap-4">
                
                {/* Modifiers Checkboxes */}
                <div className="flex items-center gap-4">
                  <label className="flex items-center gap-2 cursor-pointer select-none text-xs text-neutral-300">
                    <input 
                      type="checkbox" 
                      checked={hotkeyCtrl} 
                      onChange={(e) => setHotkeyCtrl(e.target.checked)}
                      className="w-4 h-4 rounded border-neutral-800 bg-neutral-900 text-indigo-600 focus:ring-0 focus:ring-offset-0"
                    />
                    <span>Ctrl</span>
                  </label>
                  
                  <label className="flex items-center gap-2 cursor-pointer select-none text-xs text-neutral-300">
                    <input 
                      type="checkbox" 
                      checked={hotkeyAlt} 
                      onChange={(e) => setHotkeyAlt(e.target.checked)}
                      className="w-4 h-4 rounded border-neutral-800 bg-neutral-900 text-indigo-600 focus:ring-0 focus:ring-offset-0"
                    />
                    <span>Alt</span>
                  </label>
                  
                  <label className="flex items-center gap-2 cursor-pointer select-none text-xs text-neutral-300">
                    <input 
                      type="checkbox" 
                      checked={hotkeyShift} 
                      onChange={(e) => setHotkeyShift(e.target.checked)}
                      className="w-4 h-4 rounded border-neutral-800 bg-neutral-900 text-indigo-600 focus:ring-0 focus:ring-offset-0"
                    />
                    <span>Shift</span>
                  </label>
                </div>

                {/* Key Dropdown Selection */}
                <div className="flex items-center justify-between gap-4">
                  <span className="text-neutral-400 text-xs font-medium shrink-0">Клавиша активации:</span>
                  <select
                    value={hotkeyKeyCode}
                    onChange={(e) => setHotkeyKeyCode(parseInt(e.target.value, 10))}
                    className="flex-1 bg-neutral-900 border border-neutral-800 rounded-xl px-3.5 py-2 text-xs text-neutral-200 outline-none hover:border-neutral-700 transition"
                  >
                    {KEYS_LIST.map((key) => (
                      <option key={key.code} value={key.code}>
                        {key.name}
                      </option>
                    ))}
                  </select>
                </div>

                {/* Preview Badge */}
                <div className="flex items-center justify-between text-xs pt-1 border-t border-neutral-900/40">
                  <span className="text-neutral-500 font-mono text-[10px] uppercase font-bold">Выбранная комбинация:</span>
                  <span className="px-2 py-0.5 rounded bg-neutral-900 border border-neutral-850 text-indigo-300 font-mono font-bold text-[10px] uppercase tracking-wider">{getHotkeyString()}</span>
                </div>

              </div>
            </div>

            {/* Section 3: iOS/Android Web Portal */}
            <div className="flex flex-col gap-3">
              <h4 className="text-[10px] font-extrabold text-indigo-400 uppercase tracking-wider flex items-center gap-1.5">
                <Wifi className="w-3.5 h-3.5" /> Мобильный Веб-портал
              </h4>
              <p className="text-xs text-neutral-400 leading-relaxed">
                Позволяет передавать файлы с iPhone, Android или планшета на этот компьютер напрямую по Wi-Fi без установки приложений на телефон.
              </p>
              <div className="bg-neutral-950/60 border border-neutral-900 rounded-2xl p-4 flex flex-col gap-3.5">
                <div className="text-[10px] text-neutral-500 font-mono uppercase tracking-wider font-bold">Адрес веб-портала в локальной сети:</div>
                <div className="flex items-center justify-between gap-3 bg-neutral-900/60 border border-neutral-850 rounded-xl px-3.5 py-2">
                  <span className="font-mono text-xs text-indigo-300 select-all truncate">
                    {`http://${activeIp}:53203`}
                  </span>
                  <button 
                    onClick={handleCopyLink}
                    className={`text-[9px] font-bold px-3 py-1.5 rounded-lg border transition-all uppercase tracking-wider flex items-center gap-1.5 ${
                      copied 
                        ? "border-emerald-500 bg-emerald-500/10 text-emerald-400" 
                        : "border-neutral-850 bg-neutral-950 hover:bg-neutral-900 text-neutral-300"
                    }`}
                  >
                    <Copy className="w-3 h-3" />
                    {copied ? "Готово!" : "Копировать"}
                  </button>
                </div>
                <div className="h-[1px] bg-neutral-900 my-1" />
                <div className="flex flex-col gap-2.5">
                  <div className="flex gap-3 items-start text-xs">
                    <div className="w-5 h-5 rounded-full bg-neutral-900 border border-neutral-800 flex items-center justify-center font-bold text-[10px] text-indigo-400 shrink-0">1</div>
                    <p className="text-neutral-400 leading-relaxed">Подключите телефон к той же Wi-Fi сети.</p>
                  </div>
                  <div className="flex gap-3 items-start text-xs">
                    <div className="w-5 h-5 rounded-full bg-neutral-900 border border-neutral-800 flex items-center justify-center font-bold text-[10px] text-indigo-400 shrink-0">2</div>
                    <p className="text-neutral-400 leading-relaxed">Откройте Safari или Chrome на телефоне и введите адрес выше.</p>
                  </div>
                  <div className="flex gap-3 items-start text-xs">
                    <div className="w-5 h-5 rounded-full bg-neutral-900 border border-neutral-800 flex items-center justify-center font-bold text-[10px] text-indigo-400 shrink-0">3</div>
                    <p className="text-neutral-400 leading-relaxed">Загрузите файлы с телефона, они сохранятся в папку <strong>«Загрузки»</strong> на ПК.</p>
                  </div>
                </div>
              </div>
            </div>

            {/* Section 3: Application Logs */}
            <div className="flex flex-col gap-3 border-t border-neutral-900 pt-5">
              <h4 className="text-[10px] font-extrabold text-indigo-400 uppercase tracking-wider flex items-center gap-1.5">
                <FileText className="w-3.5 h-3.5" /> Логи приложения
              </h4>
              <p className="text-xs text-neutral-400 leading-relaxed">
                Вы можете сохранить файл логов (.txt) для анализа ошибок и крашей, скопировать их в буфер обмена или очистить.
              </p>
              <div className="bg-neutral-950/60 border border-neutral-900 rounded-2xl p-4 flex flex-col gap-3">
                <div className="flex gap-2.5 w-full">
                  <button 
                    onClick={handleDownloadLogs}
                    className="flex-1 inline-flex items-center justify-center gap-2 px-3 py-2.5 rounded-xl bg-indigo-600 hover:bg-indigo-500 text-white text-[10px] font-bold uppercase tracking-wider transition-all shadow-md shadow-indigo-900/10 active-depress"
                  >
                    <FileText className="w-3.5 h-3.5" />
                    Скачать логи
                  </button>
                  <button 
                    onClick={handleCopyLogs}
                    className={`flex-1 inline-flex items-center justify-center gap-2 px-3 py-2.5 rounded-xl border text-[10px] font-bold uppercase tracking-wider transition-all active-depress ${
                      copiedLogs 
                        ? "border-emerald-500 bg-emerald-500/10 text-emerald-400" 
                        : "border-neutral-850 bg-neutral-900 hover:bg-neutral-800 text-neutral-300"
                    }`}
                  >
                    <Copy className="w-3.5 h-3.5" />
                    {copiedLogs ? "Готово!" : "Копировать"}
                  </button>
                  <button 
                    onClick={handleClearLogs}
                    className={`flex-1 inline-flex items-center justify-center gap-2 px-3 py-2.5 rounded-xl border text-[10px] font-bold uppercase tracking-wider transition-all active-depress ${
                      clearedLogs
                        ? "border-emerald-500 bg-emerald-500/10 text-emerald-400"
                        : "border-neutral-850 bg-neutral-900 hover:bg-neutral-800 text-neutral-300"
                    }`}
                  >
                    <Trash2 className="w-3.5 h-3.5" />
                    {clearedLogs ? "Очищено!" : "Очистить"}
                  </button>
                </div>
              </div>
            </div>

            {/* Section 4: About Developer */}
            <div className="flex flex-col gap-3 border-t border-neutral-900 pt-5">
              <h4 className="text-[10px] font-extrabold text-indigo-400 uppercase tracking-wider">О разработчике</h4>
              <div className="bg-neutral-950/40 border border-neutral-900 rounded-2xl p-5 flex flex-col items-center text-center relative overflow-hidden">
                <div className="absolute top-1/2 left-1/2 -translate-x-1/2 -translate-y-1/2 w-48 h-48 rounded-full bg-indigo-500/5 blur-[50px] pointer-events-none" />
                <div className="w-14 h-14 rounded-2xl bg-gradient-to-tr from-indigo-500 to-purple-600 flex items-center justify-center shadow-lg shadow-indigo-500/10 font-black text-xl text-white mb-4">
                  MD
                </div>
                <div className="font-bold text-sm text-neutral-100">Максимов Д.А.</div>
                <div className="text-[10px] text-neutral-500 font-mono tracking-wider mb-5">Lead Developer & Architect</div>
                <div className="flex gap-3 w-full justify-center">
                  <a 
                    href="https://t.me/dmitrymx" 
                    target="_blank" 
                    rel="noopener noreferrer"
                    className="flex-1 max-w-[140px] inline-flex items-center justify-center gap-2 px-4 py-2.5 rounded-xl bg-[#24A1DE] hover:bg-[#208ec4] text-white text-[10px] font-bold uppercase tracking-wider transition-all shadow-md shadow-cyan-900/10 active-depress"
                  >
                    Telegram
                  </a>
                  <a 
                    href="https://mxmvdev.ru" 
                    target="_blank" 
                    rel="noopener noreferrer"
                    className="flex-1 max-w-[140px] inline-flex items-center justify-center gap-2 px-4 py-2.5 rounded-xl bg-neutral-900 hover:bg-neutral-800 border border-neutral-850 hover:border-neutral-700 text-neutral-300 hover:text-white text-[10px] font-bold uppercase tracking-wider transition-all active-depress"
                  >
                    Сайт
                  </a>
                </div>
              </div>
            </div>

          </div>
        </div>
      </dialog>

      {/* Footer */}
      <footer className="py-5 border-t border-neutral-950 bg-neutral-950/40 text-center text-[10px] text-neutral-600 font-mono tracking-wider z-10">
        DeskBridge v2.2.0 • Прямое P2P и KVM по локальной сети без облачных серверов
      </footer>
    </div>
  );
}

export default App;
