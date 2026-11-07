import Brightness4Icon from "@mui/icons-material/Brightness4";
import Brightness7Icon from "@mui/icons-material/Brightness7";
import AppBar from "@mui/material/AppBar";
import Box from "@mui/material/Box";
import CssBaseline from "@mui/material/CssBaseline";
import IconButton from "@mui/material/IconButton";
import { ThemeProvider } from "@mui/material/styles";
import Tab from "@mui/material/Tab";
import Tabs from "@mui/material/Tabs";
import Toolbar from "@mui/material/Toolbar";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import useMediaQuery from "@mui/material/useMediaQuery";
import { useCallback, useMemo, useState } from "react";
import { BrowserRouter, Link, Navigate, Route, Routes, useLocation } from "react-router";
import Chat from "./pages/Chat";
import Clients from "./pages/Clients";
import { buildTheme, readStoredMode, storeMode, type ThemeMode } from "./theme";

/** Top bar tabs, in display order. */
const NAV = [
  { label: "Chat", path: "/chat" },
  { label: "Clients", path: "/clients" },
] as const;

/**
 * Follows the browser/OS preference live until the user explicitly toggles,
 * after which their choice is persisted and wins.
 */
function useThemeMode(): [ThemeMode, () => void] {
  // noSsr: there's no server render, so read the real value on the first
  // render instead of flashing light first. useMediaQuery subscribes to the
  // query's `change` event, so an OS theme switch is picked up live.
  const prefersDark = useMediaQuery("(prefers-color-scheme: dark)", { noSsr: true });
  const [override, setOverride] = useState<ThemeMode | null>(readStoredMode);
  const mode = override ?? (prefersDark ? "dark" : "light");

  const toggle = useCallback(() => {
    const next = mode === "dark" ? "light" : "dark";
    storeMode(next);
    setOverride(next);
  }, [mode]);

  return [mode, toggle];
}

function NavTabs() {
  const { pathname } = useLocation();
  // Prefix match so nested routes (e.g. a future /chat/:id) keep their tab lit.
  const current = NAV.find((n) => pathname === n.path || pathname.startsWith(`${n.path}/`));
  return (
    <Tabs
      value={current?.path ?? false}
      textColor="inherit"
      sx={{
        ml: 3,
        alignSelf: "stretch",
        // Stretch every layer down to the tabs so they fill the bar's height.
        "& .MuiTabs-scroller, & .MuiTabs-list, & .MuiTabs-flexContainer": { height: "100%" },
        "& .MuiTabs-indicator": { bgcolor: "common.white" },
      }}
    >
      {NAV.map((n) => (
        <Tab
          key={n.path}
          label={n.label}
          value={n.path}
          component={Link}
          to={n.path}
          sx={{ height: "100%", justifyContent: "flex-end", pb: 2 }}
        />
      ))}
    </Tabs>
  );
}

export default function App() {
  const [mode, toggleMode] = useThemeMode();
  const theme = useMemo(() => buildTheme(mode), [mode]);

  return (
    <ThemeProvider theme={theme}>
      <CssBaseline enableColorScheme />
      <BrowserRouter>
        {/* Full-viewport column: pages fill what's left under the bar and
            scroll inside <main>, so the chat page can pin its composer to
            the bottom without the whole document scrolling. */}
        <Box sx={{ display: "flex", flexDirection: "column", height: "100dvh" }}>
          <AppBar position="static">
            <Toolbar variant="dense">
              <Typography
                variant="h6"
                component="h1"
              >
                Delegation
              </Typography>
              <NavTabs />
              <Box sx={{ flexGrow: 1 }} />
              <Tooltip title={mode === "dark" ? "Switch to light mode" : "Switch to dark mode"}>
                <IconButton
                  color="inherit"
                  onClick={toggleMode}
                  aria-label="Toggle light/dark mode"
                >
                  {mode === "dark" ? <Brightness7Icon /> : <Brightness4Icon />}
                </IconButton>
              </Tooltip>
            </Toolbar>
          </AppBar>

          <Box
            component="main"
            sx={{ flex: 1, minHeight: 0, overflow: "auto" }}
          >
            <Routes>
              <Route
                path="/"
                element={
                  <Navigate
                    to="/clients"
                    replace
                  />
                }
              />
              <Route
                path="/chat"
                element={<Chat />}
              />
              <Route
                path="/clients"
                element={<Clients />}
              />
              <Route
                path="*"
                element={
                  <Navigate
                    to="/"
                    replace
                  />
                }
              />
            </Routes>
          </Box>
        </Box>
      </BrowserRouter>
    </ThemeProvider>
  );
}
