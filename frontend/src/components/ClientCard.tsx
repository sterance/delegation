import { useState } from "react";
import ExpandMoreIcon from "@mui/icons-material/ExpandMore";
import Box from "@mui/material/Box";
import Card from "@mui/material/Card";
import CardContent from "@mui/material/CardContent";
import Collapse from "@mui/material/Collapse";
import IconButton from "@mui/material/IconButton";
import Stack from "@mui/material/Stack";
import Typography from "@mui/material/Typography";
import type { Client } from "../api";

const timestampFormat = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "medium" });

function formatTimestamp(iso: string | null): string {
  if (!iso) return "Never";
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : timestampFormat.format(date);
}

function StatusDot({ connected }: { connected: boolean }) {
  const label = connected ? "Connected" : "Disconnected";
  return (
    <Box
      component="span"
      role="img"
      aria-label={label}
      title={label}
      sx={{
        width: 10,
        height: 10,
        borderRadius: "50%",
        flexShrink: 0,
        bgcolor: connected ? "success.main" : "error.main",
      }}
    />
  );
}

function Timestamp({ label, iso }: { label: string; iso: string | null }) {
  return (
    <Stack
      direction="row"
      spacing={1}
      sx={{ justifyContent: "space-between" }}
    >
      <Typography
        variant="body2"
        color="text.secondary"
      >
        {label}
      </Typography>
      <Typography
        variant="body2"
        title={iso ?? undefined}
      >
        {formatTimestamp(iso)}
      </Typography>
    </Stack>
  );
}

export default function ClientCard({ client }: { client: Client }) {
  const [expanded, setExpanded] = useState(false);
  const detailsId = `client-${client.client_id}-details`;

  return (
    <Card variant="outlined">
      <CardContent sx={{ display: "flex", alignItems: "center", gap: 2, "&:last-child": { pb: 2 } }}>
        <StatusDot connected={client.connected} />
        <Box sx={{ flexGrow: 1, minWidth: 0 }}>
          <Typography
            variant="body1"
            noWrap
            sx={{ fontWeight: 500 }}
          >
            {client.hostname}
          </Typography>
          <Typography
            variant="caption"
            color="text.secondary"
            noWrap
            component="div"
            sx={{ fontFamily: "monospace" }}
          >
            {client.client_id}
          </Typography>
        </Box>
        <IconButton
          onClick={() => setExpanded((v) => !v)}
          aria-expanded={expanded}
          aria-controls={detailsId}
          aria-label={expanded ? "Hide details" : "Show details"}
          size="small"
        >
          <ExpandMoreIcon
            sx={{
              transform: expanded ? "rotate(180deg)" : "none",
              transition: (theme) => theme.transitions.create("transform"),
            }}
          />
        </IconButton>
      </CardContent>
      <Collapse
        in={expanded}
        timeout="auto"
        unmountOnExit
        id={detailsId}
      >
        <CardContent sx={{ pt: 0, "&:last-child": { pb: 2 } }}>
          <Stack spacing={0.5}>
            <Timestamp
              label="Enrolled"
              iso={client.enrolled_at}
            />
            <Timestamp
              label="Last seen"
              iso={client.last_seen_at}
            />
          </Stack>
        </CardContent>
      </Collapse>
    </Card>
  );
}
