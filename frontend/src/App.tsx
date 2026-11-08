import AccountCircleIcon from "@mui/icons-material/AccountCircle";
import KeyIcon from "@mui/icons-material/Key";
import LogoutIcon from "@mui/icons-material/Logout";
import AppBar from "@mui/material/AppBar";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import CssBaseline from "@mui/material/CssBaseline";
import Divider from "@mui/material/Divider";
import ListItemIcon from "@mui/material/ListItemIcon";
import Menu from "@mui/material/Menu";
import MenuItem from "@mui/material/MenuItem";
import { ThemeProvider } from "@mui/material/styles";
import Tab from "@mui/material/Tab";
import Tabs from "@mui/material/Tabs";
import Toolbar from "@mui/material/Toolbar";
import Typography from "@mui/material/Typography";
import useMediaQuery from "@mui/material/useMediaQuery";
import { useCallback, useMemo, useState } from "react";
import { BrowserRouter, Link, Navigate, Outlet, Route, Routes, useLocation, useNavigate } from "react-router";
import AuthProvider from "./auth/AuthProvider";
import RequireAuth from "./auth/RequireAuth";
import { useAuth } from "./auth/context";
import ChangePasswordModal from "./components/auth/ChangePasswordModal";
import ThemeToggle from "./components/common/ThemeToggle";
import Chat from "./pages/Chat";
import Clients from "./pages/Clients";
import Login from "./pages/Login";
import ResetPassword from "./pages/ResetPassword";
import Setup from "./pages/Setup";
import Users from "./pages/Users";
import { buildTheme, readStoredMode, storeMode, type ThemeMode } from "./theme";
import { ThemeModeContext } from "./themeMode";

/** Top bar tabs, in display order. `admin` ones only show for the admin. */
const NAV = [
  { label: "Clients", path: "/clients", admin: false },
  { label: "Chat", path: "/chat", admin: false },
  { label: "Users", path: "/users", admin: true },
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

function NavTabs({ isAdmin }: { isAdmin: boolean }) {
  const { pathname } = useLocation();
  const items = NAV.filter((n) => isAdmin || !n.admin);
  // Prefix match so nested routes (e.g. a future /chat/:id) keep their tab lit.
  const current = items.find((n) => pathname === n.path || pathname.startsWith(`${n.path}/`));
  return (
    <Tabs
      value={current?.path ?? false}
      textColor="inherit"
      variant="scrollable"
      scrollButtons={false}
      sx={{
        ml: { xs: 1, sm: 3 },
        alignSelf: "stretch",
        minWidth: 0,
        "& .MuiTabs-scroller, & .MuiTabs-list, & .MuiTabs-flexContainer": { height: "100%" },
        "& .MuiTabs-indicator": { bgcolor: "common.white" },
      }}
    >
      {items.map((n) => (
        <Tab
          key={n.path}
          label={n.label}
          value={n.path}
          component={Link}
          to={n.path}
          sx={{ height: "100%", justifyContent: "flex-end", pb: 2, minWidth: { xs: 64, sm: 90 } }}
        />
      ))}
    </Tabs>
  );
}

/** The signed-in user's name, with "Change password" and "Sign out". */
function UserMenu() {
  const { user, status, signOut } = useAuth();
  const navigate = useNavigate();
  const [anchor, setAnchor] = useState<HTMLElement | null>(null);
  const [passwordOpen, setPasswordOpen] = useState(false);
  if (!user) return null;
  return (
    <>
      <Button
        color="inherit"
        onClick={(e) => setAnchor(e.currentTarget)}
        startIcon={<AccountCircleIcon />}
        aria-haspopup="menu"
        aria-label={`Account menu for ${user.username}`}
        sx={{ textTransform: "none", minWidth: 0, "& .MuiButton-startIcon": { mr: { xs: 0, sm: 1 } } }}
      >
        <Box
          component="span"
          sx={{ display: { xs: "none", sm: "inline" }, maxWidth: 160, overflow: "hidden", textOverflow: "ellipsis" }}
        >
          {user.username}
        </Box>
      </Button>
      <Menu
        anchorEl={anchor}
        open={anchor !== null}
        onClose={() => setAnchor(null)}
        anchorOrigin={{ vertical: "bottom", horizontal: "right" }}
        transformOrigin={{ vertical: "top", horizontal: "right" }}
      >
        <Box sx={{ px: 2, py: 1 }}>
          <Typography variant="body2">{user.username}</Typography>
          <Typography
            variant="caption"
            color="text.secondary"
          >
            {user.role === "admin" ? "Site admin" : "Signed in"}
          </Typography>
        </Box>
        <Divider />
        <MenuItem
          onClick={() => {
            setAnchor(null);
            setPasswordOpen(true);
          }}
        >
          <ListItemIcon>
            <KeyIcon fontSize="small" />
          </ListItemIcon>
          Change password
        </MenuItem>
        <MenuItem
          onClick={() => {
            setAnchor(null);
            void signOut().finally(() => navigate("/login", { replace: true }));
          }}
        >
          <ListItemIcon>
            <LogoutIcon fontSize="small" />
          </ListItemIcon>
          Sign out
        </MenuItem>
      </Menu>
      <ChangePasswordModal
        open={passwordOpen}
        onClose={() => setPasswordOpen(false)}
        username={user.username}
        minLength={status?.password_min_len ?? 12}
      />
    </>
  );
}

/** The signed-in app: the bar, then the page. */
function Shell() {
  const { user } = useAuth();
  return (
    // Full-viewport column: pages fill what's left under the bar and
    // scroll inside <main>, so the chat page can pin its composer to the
    // bottom without the whole document scrolling.
    <Box sx={{ display: "flex", flexDirection: "column", height: "100dvh" }}>
      <AppBar position="static">
        <Toolbar
          variant="dense"
          sx={{ px: { xs: 1, sm: 2 } }}
        >
          <Typography
            variant="h6"
            component="h1"
            sx={{ display: { xs: "none", sm: "block" } }}
          >
            Delegation
          </Typography>
          <NavTabs isAdmin={user?.role === "admin"} />
          <Box sx={{ flexGrow: 1 }} />
          <ThemeToggle color="inherit" />
          <UserMenu />
        </Toolbar>
      </AppBar>

      <Box
        component="main"
        sx={{ flex: 1, minHeight: 0, overflow: "auto" }}
      >
        <Outlet />
      </Box>
    </Box>
  );
}

export default function App() {
  const [mode, toggleMode] = useThemeMode();
  const theme = useMemo(() => buildTheme(mode), [mode]);
  const themeMode = useMemo(() => ({ mode, toggle: toggleMode }), [mode, toggleMode]);

  return (
    <ThemeProvider theme={theme}>
      <CssBaseline enableColorScheme />
      <ThemeModeContext.Provider value={themeMode}>
        <BrowserRouter>
          <AuthProvider>
            <Routes>
              <Route
                path="/login"
                element={<Login />}
              />
              <Route
                path="/setup"
                element={<Setup />}
              />
              <Route
                path="/reset/:token"
                element={<ResetPassword />}
              />
              <Route
                element={
                  <RequireAuth>
                    <Shell />
                  </RequireAuth>
                }
              >
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
                  path="/clients"
                  element={<Clients />}
                />
                <Route
                  path="/chat"
                  element={<Chat />}
                />
                <Route
                  path="/users"
                  element={
                    <RequireAuth admin>
                      <Users />
                    </RequireAuth>
                  }
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
              </Route>
            </Routes>
          </AuthProvider>
        </BrowserRouter>
      </ThemeModeContext.Provider>
    </ThemeProvider>
  );
}
