import { createRoot } from "react-dom/client";
import App from "./App";
import "./style.css";

createRoot(document.querySelector<HTMLDivElement>("#app")!).render(<App />);
