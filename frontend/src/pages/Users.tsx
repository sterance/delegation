import BlockIcon from "@mui/icons-material/Block";
import CheckCircleOutlinedIcon from "@mui/icons-material/CheckCircleOutlined";
import ContentCopyIcon from "@mui/icons-material/ContentCopy";
import DeleteOutlinedIcon from "@mui/icons-material/DeleteOutlined";
import KeyIcon from "@mui/icons-material/Key";
import Alert from "@mui/material/Alert";
import Box from "@mui/material/Box";
import Button from "@mui/material/Button";
import Chip from "@mui/material/Chip";
import CircularProgress from "@mui/material/CircularProgress";
import Container from "@mui/material/Container";
import Dialog from "@mui/material/Dialog";
import DialogActions from "@mui/material/DialogActions";
import DialogContent from "@mui/material/DialogContent";
import DialogContentText from "@mui/material/DialogContentText";
import DialogTitle from "@mui/material/DialogTitle";
import IconButton from "@mui/material/IconButton";
import InputAdornment from "@mui/material/InputAdornment";
import Paper from "@mui/material/Paper";
import Stack from "@mui/material/Stack";
import Table from "@mui/material/Table";
import TableBody from "@mui/material/TableBody";
import TableCell from "@mui/material/TableCell";
import TableContainer from "@mui/material/TableContainer";
import TableHead from "@mui/material/TableHead";
import TableRow from "@mui/material/TableRow";
import TextField from "@mui/material/TextField";
import Tooltip from "@mui/material/Tooltip";
import Typography from "@mui/material/Typography";
import { useCallback, useEffect, useState } from "react";
import { createResetLink, deleteUser, errorMessage, listUsers, setUserDisabled, type ResetLink, type UserSummary, type UsersResponse } from "../api";
import { useAuth } from "../auth/context";
import ConfirmationModal from "../components/common/ConfirmationModal";

const dateFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

function formatDate(iso: string | null): string {
  if (!iso) return "Never";
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : dateFormat.format(date);
}

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** The admin's list of accounts: disable, delete, and password reset links. */
export default function Users() {
  const { user: me } = useAuth();
  const [data, setData] = useState<UsersResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [link, setLink] = useState<{ user: UserSummary; link: ResetLink } | null>(null);
  const [linkOpen, setLinkOpen] = useState(false);
  const [toDelete, setToDelete] = useState<UserSummary | null>(null);
  const [deleteOpen, setDeleteOpen] = useState(false);

  const refresh = useCallback(
    () =>
      listUsers().then(
        (users) => {
          setData(users);
          setError(null);
        },
        (err: unknown) => setError(errorMessage(err)),
      ),
    [],
  );
  useEffect(() => {
    let cancelled = false;
    listUsers().then(
      (users) => {
        if (!cancelled) setData(users);
      },
      (err: unknown) => {
        if (!cancelled) setError(errorMessage(err));
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  const act = async (id: string, action: () => Promise<void>) => {
    setBusy(id);
    try {
      await action();
      await refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(null);
    }
  };

  const makeLink = (user: UserSummary) =>
    act(user.id, async () => {
      setLink({ user, link: await createResetLink(user.id) });
      setLinkOpen(true);
    });

  return (
    <Container
      maxWidth="lg"
      sx={{ py: 4 }}
    >
      <Box sx={{ mb: 2 }}>
        <Typography
          variant="h5"
          component="h2"
        >
          Users
        </Typography>
        {data && (
          <Typography
            variant="body2"
            color="text.secondary"
          >
            {plural(data.users.length, "account", "accounts")}, newest first
          </Typography>
        )}
      </Box>

      {/* {data && (
        <Alert severity={data.signup_enabled ? 'info' : 'success'} sx={{ mb: 2 }}>
          {data.signup_enabled
            ? 'Sign-up is open: anyone who can reach this site can create an account. Set AUTH_SIGNUP_ENABLED=false in config.env to turn it off.'
            : 'Sign-up is turned off (AUTH_SIGNUP_ENABLED=false). Only existing accounts can sign in.'}
        </Alert>
      )} */}
      {error && (
        <Alert
          severity="error"
          sx={{ mb: 2 }}
          onClose={() => setError(null)}
        >
          {error}
        </Alert>
      )}

      {!data ? (
        !error && (
          <Box sx={{ display: "flex", justifyContent: "center", py: 6 }}>
            <CircularProgress aria-label="Loading users" />
          </Box>
        )
      ) : (
        <TableContainer
          component={Paper}
          variant="outlined"
        >
          <Table
            size="small"
            aria-label="Users"
          >
            <TableHead>
              <TableRow>
                <TableCell>Username</TableCell>
                <TableCell>Created</TableCell>
                <TableCell>Last active</TableCell>
                <TableCell align="right">Clients</TableCell>
                <TableCell align="right">Chats</TableCell>
                <TableCell align="right">Actions</TableCell>
              </TableRow>
            </TableHead>
            <TableBody>
              {data.users.map((u) => {
                const self = u.id === me?.id;
                return (
                  <TableRow
                    key={u.id}
                    sx={{ opacity: u.disabled_at ? 0.6 : 1 }}
                  >
                    <TableCell>
                      <Stack
                        direction="row"
                        spacing={1}
                        sx={{ alignItems: "center" }}
                      >
                        <span>{u.username}</span>
                        {u.role === "admin" && (
                          <Chip
                            size="small"
                            label="admin"
                            color="primary"
                            variant="outlined"
                          />
                        )}
                        {self && (
                          <Chip
                            size="small"
                            label="you"
                            variant="outlined"
                          />
                        )}
                        {u.disabled_at && (
                          <Chip
                            size="small"
                            label="disabled"
                            color="warning"
                            variant="outlined"
                          />
                        )}
                      </Stack>
                    </TableCell>
                    <TableCell sx={{ whiteSpace: "nowrap" }}>{formatDate(u.created_at)}</TableCell>
                    <TableCell sx={{ whiteSpace: "nowrap" }}>{formatDate(u.last_active_at)}</TableCell>
                    <TableCell align="right">{u.client_count}</TableCell>
                    <TableCell align="right">{u.conversation_count}</TableCell>
                    <TableCell
                      align="right"
                      sx={{ whiteSpace: "nowrap" }}
                    >
                      {busy === u.id && (
                        <CircularProgress
                          size={18}
                          sx={{ mr: 1, verticalAlign: "middle" }}
                        />
                      )}
                      {!self && (
                        <>
                          <Tooltip title="Password reset link">
                            <span>
                              <IconButton
                                size="small"
                                onClick={() => void makeLink(u)}
                                disabled={busy !== null}
                                aria-label={`Password reset link for ${u.username}`}
                              >
                                <KeyIcon fontSize="small" />
                              </IconButton>
                            </span>
                          </Tooltip>
                          <Tooltip title={u.disabled_at ? "Enable" : "Disable"}>
                            <span>
                              <IconButton
                                size="small"
                                onClick={() => void act(u.id, () => setUserDisabled(u.id, !u.disabled_at))}
                                disabled={busy !== null}
                                aria-label={`${u.disabled_at ? "Enable" : "Disable"} ${u.username}`}
                              >
                                {u.disabled_at ? <CheckCircleOutlinedIcon fontSize="small" /> : <BlockIcon fontSize="small" />}
                              </IconButton>
                            </span>
                          </Tooltip>
                          <Tooltip title="Delete">
                            <span>
                              <IconButton
                                size="small"
                                onClick={() => {
                                  setToDelete(u);
                                  setDeleteOpen(true);
                                }}
                                disabled={busy !== null}
                                aria-label={`Delete ${u.username}`}
                              >
                                <DeleteOutlinedIcon fontSize="small" />
                              </IconButton>
                            </span>
                          </Tooltip>
                        </>
                      )}
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </TableContainer>
      )}

      <ResetLinkDialog
        open={linkOpen}
        value={link}
        onClose={() => setLinkOpen(false)}
      />

      <ConfirmationModal
        open={deleteOpen}
        title={`Delete ${toDelete?.username ?? "this user"}?`}
        message={
          <Stack spacing={1.5}>
            <DialogContentText>
              This deletes their {plural(toDelete?.client_count ?? 0, "client", "clients")} and {plural(toDelete?.conversation_count ?? 0, "conversation", "conversations")}, and can't be undone. Connected clients are disconnected and their keys stop working.
            </DialogContentText>
            <DialogContentText>
              To stop them signing in but keep everything, <strong>disable</strong> the account instead.
            </DialogContentText>
          </Stack>
        }
        confirmLabel="Delete"
        confirmColor="error"
        onConfirm={async () => {
          if (!toDelete) return;
          await deleteUser(toDelete.id);
          await refresh();
        }}
        onClose={() => setDeleteOpen(false)}
      />
    </Container>
  );
}

function ResetLinkDialog({ open, value, onClose }: { open: boolean; value: { user: UserSummary; link: ResetLink } | null; onClose: () => void }) {
  const [copied, setCopied] = useState<"idle" | "copied" | "failed">("idle");
  const [wasOpen, setWasOpen] = useState(open);
  if (open !== wasOpen) {
    setWasOpen(open);
    if (open) setCopied("idle");
  }
  const copy = async () => {
    if (!value) return;
    try {
      // Only in secure contexts (https or localhost).
      await navigator.clipboard.writeText(value.link.url);
      setCopied("copied");
    } catch {
      setCopied("failed");
    }
  };
  return (
    <Dialog
      open={open}
      onClose={onClose}
      maxWidth="sm"
      fullWidth
      aria-labelledby="reset-link-title"
    >
      <DialogTitle id="reset-link-title">Reset link for {value?.user.username}</DialogTitle>
      <DialogContent>
        <DialogContentText sx={{ mb: 2 }}>
          Pass this link on to {value?.user.username}. It works once, until {value ? formatDate(value.link.expires_at) : ""}, and signs them out everywhere when used. This is the only time it's shown; any older link for them no longer works.
        </DialogContentText>
        <TextField
          value={value?.link.url ?? ""}
          fullWidth
          size="small"
          onFocus={(e) => e.target.select()}
          slotProps={{
            htmlInput: { readOnly: true, "aria-label": "Reset link" },
            input: {
              endAdornment: (
                <InputAdornment position="end">
                  <Tooltip title={copied === "copied" ? "Copied!" : copied === "failed" ? "Couldn't copy — select it manually" : "Copy"}>
                    <IconButton
                      onClick={() => void copy()}
                      edge="end"
                      aria-label="Copy reset link"
                    >
                      <ContentCopyIcon fontSize="small" />
                    </IconButton>
                  </Tooltip>
                </InputAdornment>
              ),
            },
          }}
        />
      </DialogContent>
      <DialogActions>
        <Button
          variant="contained"
          onClick={onClose}
        >
          Done
        </Button>
      </DialogActions>
    </Dialog>
  );
}
