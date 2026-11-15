export interface Props {
  onPick(id: number): void;
  label: string;
}

export function View(props: Props) {
  return <button onClick={() => props.onPick(1)}>{props.label}</button>;
}
