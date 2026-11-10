import React from "react";
import { ChevronDown, ChevronRight } from "lucide-react";

interface CardCollapseToggleProps {
  open: boolean;
  onToggle: () => void;
  title: string;
  count?: number;
  controlsId?: string;
}

/**
 * Header toggle for Settings cards that hold operational data (signature
 * tables, enrolled galleries, plate registries) rather than configuration.
 * The card starts collapsed and shows how many items it holds.
 */
export const CardCollapseToggle: React.FC<CardCollapseToggleProps> = ({
  open,
  onToggle,
  title,
  count,
  controlsId,
}) => (
  <button
    type="button"
    className="nvr-card-toggle"
    aria-expanded={open}
    aria-controls={controlsId}
    onClick={onToggle}
  >
    {open ? <ChevronDown size={12} aria-hidden="true" /> : <ChevronRight size={12} aria-hidden="true" />}
    <span>{title}</span>
    {typeof count === "number" && <span className="nvr-badge">{count}</span>}
  </button>
);
